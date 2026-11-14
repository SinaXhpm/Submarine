// "Send feedback": a message to the app's developer, sent by the backend
// (src-tauri/src/feedback.rs) to Submarine's own API. Only what the person
// types goes: the message, and a name and an email if they give them. No
// logs, no app or system facts, no account.
//
// One form for the whole app, opened from Settings → Help & feedback, the
// profile list's footer and Settings → Language → Suggest a fix (which starts
// the message with the language). FeedbackHost is mounted once in App.tsx;
// openFeedback() opens it from anywhere. What is typed stays while the app
// runs, also when the form is closed (in memory only: it may hold private
// things), and is cleared once sent.
//
// Lengths are Unicode code points, counted as the backend and the server count
// them (see feedback.rs): a Chinese character or an emoji is one, like a
// letter. A text over a limit is never cut: the counter turns red and Send
// waits.
import { useEffect, useRef, useState, useSyncExternalStore } from "react";
import { invoke } from "@tauri-apps/api/core";
import { ExternalLink, Loader2, MessageSquare, X } from "lucide-react";
import { currentLanguage, tn, tx, useT } from "../i18n";
import { PRIVACY_URL, REPO_ISSUES_URL } from "../util/links";
import { useSlideOverFocus } from "../ui/slideOverFocus";
import CopyValue from "./CopyValue";

export const MESSAGE_MIN = 5;
export const MESSAGE_MAX = 4000;
export const NAME_MAX = 64;
export const EMAIL_MAX = 254;

type Outcome =
  | { status: "sent"; id: string }
  | { status: "bad_field"; field: string }
  | { status: "rate_limited"; retry_after: number | null }
  | { status: "inbox_full" }
  | { status: "blocked" }
  | { status: "offline" }
  | { status: "server"; code: number };

// The backend's cleaning (feedback.rs `clean`): line breaks as \n, control
// characters other than line breaks and tabs left out, trimmed.
// eslint-disable-next-line no-control-regex
const CONTROL = /[\u0000-\u0008\u000B-\u001F\u007F-\u009F]/g;
export const cleanText = (s: string, singleLine = false): string => {
  let out = s.replace(/\r\n?/g, "\n").replace(CONTROL, "");
  if (singleLine) out = out.replace(/[\n\t]/g, " ");
  return out.trim();
};
export const codePoints = (s: string): number => Array.from(s).length;
const emailShaped = (e: string): boolean => {
  const at = e.lastIndexOf("@");
  if (at <= 0) return false;
  const domain = e.slice(at + 1);
  return !!domain && domain.includes(".") && !domain.startsWith(".") && !domain.endsWith(".") && !/[\s\u0000-\u001F\u007F-\u009F]/.test(e);
};

const formats = new Map<string, Intl.NumberFormat>();
const numberFormat = (lang: string): Intl.NumberFormat => {
  let f = formats.get(lang);
  if (!f) {
    try { f = new Intl.NumberFormat(lang); } catch { f = new Intl.NumberFormat("en"); }
    formats.set(lang, f);
  }
  return f;
};

// ---- the open state and the draft, shared by every opener
type Draft = { message: string; name: string; email: string };
let draft: Draft = { message: "", name: "", email: "" };
let open = false;
const listeners = new Set<() => void>();
const emit = () => listeners.forEach((l) => l());
const subscribe = (l: () => void) => { listeners.add(l); return () => { listeners.delete(l); }; };

/** Opens the form. `prefill` starts an empty message (Suggest a fix: the language). */
export function openFeedback(opts: { prefill?: string } = {}) {
  if (opts.prefill && !draft.message.trim()) draft = { ...draft, message: opts.prefill };
  open = true;
  emit();
}
const closeFeedback = () => { open = false; emit(); };

export function FeedbackHost() {
  const t = useT();
  const isOpen = useSyncExternalStore(subscribe, () => open);
  const [form, setForm] = useState<Draft>(draft);
  const [sending, setSending] = useState(false);
  const [result, setResult] = useState<Outcome | null>(null);
  const [linkError, setLinkError] = useState("");
  const panelRef = useSlideOverFocus(isOpen);
  const messageRef = useRef<HTMLTextAreaElement | null>(null);

  // The shared draft is the truth while closed; the form's state while open.
  useEffect(() => {
    if (isOpen) {
      setForm(draft);
      setLinkError("");
      if (result?.status !== "sent") setResult(null);
      // After the slide-in, the message box (at its end, after a prefill).
      const id = window.setTimeout(() => {
        const box = messageRef.current;
        if (!box) return;
        box.focus({ preventScroll: true });
        box.setSelectionRange(box.value.length, box.value.length);
      }, 320);
      return () => window.clearTimeout(id);
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [isOpen]);
  const update = (patch: Partial<Draft>) => {
    setForm((f) => {
      const next = { ...f, ...patch };
      draft = next;
      return next;
    });
    if (result && result.status !== "sent") setResult(null);
  };

  const message = cleanText(form.message);
  const name = cleanText(form.name, true);
  const email = form.email.trim();
  const msgCount = codePoints(message);
  const nameCount = codePoints(name);
  const msgShort = msgCount < MESSAGE_MIN;
  const msgLong = msgCount > MESSAGE_MAX;
  const nameLong = nameCount > NAME_MAX;
  // An address's limit is in bytes (RFC 5321), as the backend and the server count it.
  const emailBad = email !== "" && (new TextEncoder().encode(email).length > EMAIL_MAX || !emailShaped(email));
  const canSend = !sending && !msgShort && !msgLong && !nameLong && !emailBad;
  const fmt = numberFormat(currentLanguage());

  const send = async () => {
    if (!canSend) return;
    setSending(true);
    setResult(null);
    try {
      const out = await invoke<Outcome>("send_feedback", { input: { message: form.message, name: form.name, email: form.email } });
      setResult(out);
      if (out.status === "sent") {
        draft = { message: "", name: draft.name, email: draft.email };
        setForm(draft);
      }
    } catch {
      setResult({ status: "server", code: 0 });
    } finally {
      setSending(false);
    }
  };

  const openLink = (url: string) => {
    setLinkError("");
    invoke("open_external_url", { url }).catch((e) => setLinkError(t("Couldn't open the link: {error}", { error: String(e) })));
  };

  const errorText = (r: Outcome): string | null => {
    switch (r.status) {
      case "bad_field":
        return r.field === "name" ? t("The name is too long.")
          : r.field === "email" ? t("That email address doesn't look right.")
          : t("The message is too short or too long.");
      case "rate_limited": {
        const minutes = Math.max(1, Math.ceil((r.retry_after ?? 3600) / 60));
        return `${t("Too many messages from this network.")} ${tn(minutes, "Try again in {count} minute.", "Try again in {count} minutes.")}`;
      }
      case "inbox_full": return t("The inbox is full for today. You can open a GitHub issue instead.");
      case "blocked": return t("The network blocked this message. Your text is still here.");
      case "offline": return t("Couldn't reach the server. Your text is still here; try again later.");
      case "server": return r.code ? t("The server couldn't take the message ({code}). Try again later.", { code: r.code }) : t("The message couldn't be sent. Try again later.");
      default: return null;
    }
  };
  const error = result && result.status !== "sent" ? errorText(result) : null;
  const sentId = result?.status === "sent" ? result.id : null;

  const field = "w-full bg-[#1a1a1e] rounded-lg px-3 text-[13px] text-white border outline-none transition-all shadow-inner";
  const border = (bad: boolean) => (bad ? "border-red-500/60 focus:border-red-400" : "border-white/10 focus:border-primary/50");

  return (
    <div
      inert={isOpen ? undefined : ""}
      className={`fixed inset-0 z-50 transition-all duration-500 flex justify-end ${isOpen ? "bg-black/60 backdrop-blur-sm" : "opacity-0 pointer-events-none"}`}
      onMouseDown={(e) => { if (e.target === e.currentTarget) closeFeedback(); }}
    >
      <div
        ref={panelRef}
        tabIndex={-1}
        // A dialog only while open: the terminal doesn't take the focus while
        // an [aria-modal] element is on the page (TerminalView), and this
        // panel stays mounted when closed.
        role={isOpen ? "dialog" : undefined}
        aria-modal={isOpen ? true : undefined}
        aria-labelledby="feedback-title"
        className={`w-full sm:max-w-[450px] bg-background border-l border-white/5 shadow-2xl transition-all duration-500 h-full flex flex-col outline-none ${isOpen ? "translate-x-0" : "translate-x-full"}`}
        onKeyDown={(e) => { if (e.key === "Escape") { e.stopPropagation(); closeFeedback(); } }}
      >
        <div className="flex justify-between items-center p-4 sm:p-6 border-b border-white/5 shrink-0">
          <div className="flex items-center gap-3 min-w-0">
            <div className="w-8 h-8 shrink-0 bg-primary/10 rounded-lg flex items-center justify-center text-primary shadow-inner border border-primary/20">
              <MessageSquare size={16} />
            </div>
            <h2 id="feedback-title" className="text-[14px] font-bold text-white tracking-tight truncate">{t("Send feedback")}</h2>
          </div>
          <button aria-label={t("Close")} onClick={closeFeedback} className="w-10 h-10 sm:w-8 sm:h-8 shrink-0 flex items-center justify-center text-zinc-500 hover:text-white bg-black border border-white/5 hover:bg-white/10 rounded-xl transition-all shadow-inner">
            <X size={16} />
          </button>
        </div>

        <form
          className="flex-1 min-h-0 overflow-y-auto p-4 sm:p-6 space-y-5"
          onSubmit={(e) => { e.preventDefault(); void send(); }}
        >
          <div className="space-y-1.5">
            <div className="flex items-baseline justify-between gap-3">
              <label htmlFor="feedback-message" className="text-[12px] font-semibold text-zinc-400">{t("Message")}</label>
              <span
                data-testid="feedback-count"
                aria-live="polite"
                className={`text-[11px] tabular-nums ${msgLong ? "text-red-400 font-bold" : msgCount > MESSAGE_MAX * 0.9 ? "text-amber-300" : "text-zinc-500"}`}
              >
                {t("{count} / {max}", { count: fmt.format(msgCount), max: fmt.format(MESSAGE_MAX) })}
              </span>
            </div>
            <textarea
              id="feedback-message"
              ref={messageRef}
              value={form.message}
              onChange={(e) => update({ message: e.target.value })}
              rows={9}
              placeholder={t("What happened, what you expected, or your idea.")}
              aria-invalid={msgLong || undefined}
              className={`${field} ${border(msgLong)} py-2.5 resize-y min-h-[160px] leading-relaxed`}
            />
            {msgLong && <p className="text-[11px] text-red-300">{t("The message is too long.")}</p>}
          </div>

          <div className="space-y-1.5">
            <div className="flex items-baseline justify-between gap-3">
              <label htmlFor="feedback-name" className="text-[12px] font-semibold text-zinc-400">{t("Your name (optional)")}</label>
              {nameCount > NAME_MAX * 0.75 && (
                <span className={`text-[11px] tabular-nums ${nameLong ? "text-red-400 font-bold" : "text-zinc-500"}`}>
                  {t("{count} / {max}", { count: fmt.format(nameCount), max: fmt.format(NAME_MAX) })}
                </span>
              )}
            </div>
            <input
              id="feedback-name"
              type="text"
              value={form.name}
              onChange={(e) => update({ name: e.target.value })}
              autoComplete="name"
              aria-invalid={nameLong || undefined}
              className={`${field} ${border(nameLong)} h-10 sm:h-9`}
            />
            {nameLong && <p className="text-[11px] text-red-300">{t("The name is too long.")}</p>}
          </div>

          <div className="space-y-1.5">
            <label htmlFor="feedback-email" className="text-[12px] font-semibold text-zinc-400">{t("Email (optional)")}</label>
            <input
              id="feedback-email"
              type="email"
              inputMode="email"
              value={form.email}
              onChange={(e) => update({ email: e.target.value })}
              autoComplete="email"
              autoCapitalize="none"
              autoCorrect="off"
              spellCheck={false}
              aria-invalid={emailBad || undefined}
              aria-describedby="feedback-email-hint"
              className={`${field} ${border(emailBad)} h-10 sm:h-9`}
            />
            <p id="feedback-email-hint" className={`text-[11px] ${emailBad ? "text-red-300" : "text-zinc-500"}`}>
              {emailBad ? t("That email address doesn't look right.") : t("Only if you want an answer.")}
            </p>
          </div>

          <p className="text-[11.5px] text-zinc-400 leading-relaxed">
            {tx("Only what you type here is sent, to the Submarine developer (submarine.sinaxhpm.com). A person reads it, so don't put passwords in it. Messages are kept for 180 days. {link}", {
              link: (
                <button type="button" onClick={() => openLink(PRIVACY_URL)} className="text-primary hover:underline">
                  {t("Privacy policy")}
                </button>
              ),
            })}
          </p>

          {error && (
            <div role="alert" className="rounded-xl border border-red-500/30 bg-red-500/10 px-3 py-2.5 text-[12px] text-red-200 space-y-2">
              <p>{error}</p>
              {result?.status === "inbox_full" && (
                <button type="button" onClick={() => openLink(REPO_ISSUES_URL)} className="inline-flex items-center gap-1.5 min-h-[40px] sm:min-h-0 px-3 py-1.5 rounded-lg border border-white/10 bg-white/5 text-zinc-100 hover:bg-white/10">
                  <ExternalLink size={12} aria-hidden="true" /> {t("Open a GitHub issue")}
                </button>
              )}
            </div>
          )}
          {sentId && (
            <div role="status" className="rounded-xl border border-emerald-500/30 bg-emerald-500/10 px-3 py-2.5 text-[12px] text-emerald-200 space-y-1.5">
              <p>{t("Sent. The report number is {id}; quote it if you write about this message again.", { id: sentId })}</p>
              <CopyValue value={sentId} className="text-[12px] font-mono text-emerald-100" />
            </div>
          )}
          {linkError && <p role="alert" className="text-[11px] text-red-300">{linkError}</p>}

          <button
            type="submit"
            disabled={!canSend}
            className="w-full min-h-[40px] h-10 rounded-xl bg-primary text-zinc-950 text-[12px] font-semibold hover:brightness-110 disabled:opacity-40 disabled:cursor-not-allowed flex items-center justify-center gap-2 transition-all"
          >
            {sending && <Loader2 size={14} className="animate-spin-brief" aria-hidden="true" />}
            {sending ? t("Sending…") : t("Send")}
          </button>
        </form>
      </div>
    </div>
  );
}
