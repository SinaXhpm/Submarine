import { useCallback, useEffect, useId, useLayoutEffect, useRef, useState } from "react";
import { createPortal } from "react-dom";
import { Check, ShieldAlert } from "lucide-react";
import { useConfirm, useTextPrompt } from "../../ui/confirm";
import { ai, errText, LEVELS, levelNote, type Level, type LevelInfo } from "../../util/aiApi";

// A server's permission level: the chip in the AI tab's bar, the list in its
// options drawer, the banner while Full access is on, and the menu of a
// server card. Only Auto and Full access ask first, and what they ask is
// written once, in useLevelChange.

/** The border and text of each level. Auto and Full access are red, like the SFTP root badge. */
const TONE: Record<Level, string> = {
  suggest: "border-white/15 text-zinc-300",
  ask_all: "border-sky-500/40 text-sky-300",
  ask: "border-emerald-500/40 text-emerald-300",
  auto: "border-rose-500/50 text-rose-300",
  full: "border-rose-500/60 text-rose-200",
};

const isRed = (l: Level | null | undefined) => l === "auto" || l === "full";

/** The level chip's colours. The background is opaque: a select's open list can take it. */
export const levelChipClass = (level: Level): string =>
  `${TONE[level]} ${level === "full" ? "bg-[#38141c]" : "bg-zinc-950"}`;

/** What a change starts from: the level in effect and the saved one (and
 *  whether the server has files only, which changes what the dialogs say).
 *  Null when it isn't known, and then Auto asks. */
export type LevelFrom = Pick<LevelInfo, "level" | "saved" | "files_only"> | null;

export interface LevelChange {
  serverUuid: string;
  serverName: string;
  from: LevelFrom;
  to: Level;
  /** The server's open session, where Full access lives. */
  sessionId: string | null;
}

/**
 * Asks what a change needs, then makes it. Resolves false when the user said
 * no; errors are thrown for the caller to show.
 * - Auto asks whenever it would be saved anew, from Full access too: Full
 *   access ends with the session, Auto stays on.
 * - Full access needs the open session and the server's name typed.
 * - The other levels ask nothing: they are all below Auto.
 */
export const useLevelChange = () => {
  const confirm = useConfirm();
  const prompt = useTextPrompt();
  return useCallback(async ({ serverUuid, serverName, from, to, sessionId }: LevelChange): Promise<boolean> => {
    const name = serverName.trim();
    // An FTP server: files only, no commands, so the dialogs speak of files.
    const filesOnly = !!from?.files_only;
    if (to === "full") {
      if (!sessionId) throw new Error("Full access needs an open session of this server.");
      // With no name to type, an empty field would turn it on.
      if (!name) throw new Error("Give the server a name first. Full access asks for it to be typed.");
      const typed = await prompt({
        title: `Full access on ${name}?`,
        message:
          (filesOnly
            ? "Every file read and change will run without asking, files that may hold secrets included. " +
              "Nothing waits for your approval, so instructions hidden in a file could be acted on too.\n\n"
            : "Every command will run without asking, sudo included where the server allows it without a password. " +
              "Nothing waits for your approval, so instructions hidden in a file or in a command's output could run too.\n\n") +
          "This is for this session only. It ends when the tab closes, on Lock and when the app quits, and the server goes back to its saved level.\n\n" +
          `Type the server's name to turn it on: ${name}`,
        placeholder: "Server name",
        okLabel: "Turn on Full access",
        validate: (v) => (v.trim() === name ? null : "Type the name exactly as shown"),
      });
      if (typed == null) return false;
      await ai.setLevel(serverUuid, "full", { sessionId, confirmName: typed });
      return true;
    }
    if (to === "auto" && from?.saved !== "auto") {
      const ok = await confirm({
        title: `Turn on Auto for ${name || "this server"}?`,
        message: filesOnly
          ? "File changes will run without asking you. Ones that look risky still ask: files that may hold secrets, " +
            "files that run at login, at boot or on a schedule, and files that control the kernel or a device. " +
            "This server runs no commands, so none can run here.\n\n" +
            "Text on the server can try to steer the assistant. A file it reads can hold instructions written for it, " +
            "and with Auto on it can act on them without asking you.\n\n" +
            "Auto stays on until you turn it off."
          : "Commands and file changes will run without asking you. Ones that look risky still ask: removal (any rm), sudo, disks, power, " +
            "the firewall, accounts, the network and package installs, scheduled jobs and the like, and changes to files that run at login, at boot or on a schedule. " +
            "These checks read how a command is written, and a command can hide what it does (a variable, a script, an archive), so with Auto on, anything may run.\n\n" +
            "Text on the server can try to steer the assistant. A file, a command's output or a web page it fetches can hold " +
            "instructions written for it, and with Auto on it can act on them without asking you.\n\n" +
            "Auto stays on until you turn it off.",
        okLabel: "Turn on Auto",
        destructive: true,
        safeDefault: true,
      });
      if (!ok) return false;
    }
    await ai.setLevel(serverUuid, to, { sessionId });
    return true;
  }, [confirm, prompt]);
};

/** The short reason in a select's option. */
const SHORT_WHY: Partial<Record<Level, string>> = { full: "needs an open session", auto: "needs a POSIX shell" };

// No Auto where no POSIX shell answered, as its checks are for commands in one;
// a files-only server (FTP) runs no commands, so Auto works on its files.
const noAuto = (info: { posix: boolean | null; files_only?: boolean | null }) => info.posix === false && !info.files_only;

/** Why a level can't be picked, or null. The current one always can: picking it changes nothing. */
const whyNot = (id: Level, info: LevelInfo, sessionId: string | null): string | null => {
  if (id === info.level) return null;
  if (id === "full" && !sessionId) return "Full access needs an open session of this server.";
  if (id === "auto" && noAuto(info)) return "Auto needs a POSIX shell, and this server doesn't have one.";
  return null;
};

/**
 * False once the component is gone. A dialog can outlive the control that
 * opened it: the options drawer closes on a click in the dialog. The change
 * still goes through, and the AI_LEVEL_EVENT that ai.setLevel sends tells
 * the parent. Only what this component shows is skipped.
 */
const useAlive = () => {
  const alive = useRef(true);
  useEffect(() => {
    alive.current = true;
    return () => { alive.current = false; };
  }, []);
  return alive;
};

interface LevelPickerProps {
  serverUuid: string;
  serverName: string;
  /** The server's open session: Full access is offered only with one. */
  sessionId: string | null;
  /** Null while it loads. */
  info: LevelInfo | null;
  /** Fresh info, read again after a change went through. */
  onChanged: (info: LevelInfo) => void;
  /** "chip": a select for a bar. "list": a button per level, for the options drawer. */
  variant: "chip" | "list";
  disabled?: boolean;
}

export const LevelPicker = ({ serverUuid, serverName, sessionId, info, onChanged, variant, disabled = false }: LevelPickerProps) => {
  const change = useLevelChange();
  const [error, setError] = useState<string | null>(null);
  // A pick while another one is asked or saved is dropped. Not `disabled`:
  // that would take the focus from the control.
  const busy = useRef(false);
  const alive = useAlive();

  useEffect(() => { setError(null); }, [serverUuid, info?.level]);

  if (!serverUuid) return null;

  const blocked = (id: Level) => (info ? whyNot(id, info, sessionId) : null);

  const pick = async (to: Level) => {
    if (!info || disabled || busy.current || to === info.level || blocked(to)) return;
    busy.current = true;
    setError(null);
    try {
      if (await change({ serverUuid, serverName, from: info, to, sessionId })) {
        const fresh = await ai.levelInfo(serverUuid, sessionId);
        if (alive.current) onChanged(fresh);
      }
    } catch (e) {
      if (alive.current) setError(errText(e));
    } finally {
      busy.current = false;
    }
  };

  const level = info?.level;
  const note = level ? levelNote(level, info?.files_only) : undefined;
  // Saved before a chat found out, Auto on a server without a POSIX shell is
  // decided as Ask: its checks are for one.
  const autoAsAsk = level === "auto" && info && noAuto(info) ? "Auto acts as Ask here: this server didn't answer with a POSIX shell, and Auto's checks are for one." : null;

  // Controlled: on Cancel or an error it shows the level it had.
  if (variant === "chip") {
    return (
      <span className="relative min-w-0 inline-flex items-center gap-1.5">
        {isRed(level) && <ShieldAlert size={11} className="absolute left-2 top-1/2 -translate-y-1/2 pointer-events-none text-rose-300" />}
        <select
          aria-label="Permission level"
          title={error ?? autoAsAsk ?? note}
          value={level ?? ""}
          disabled={disabled || !info}
          onChange={(e) => void pick(e.target.value as Level)}
          className={`shrink-0 h-10 sm:h-7 [field-sizing:content] max-w-[6.5rem] sm:max-w-[10rem] ${isRed(level) ? "pl-6" : "pl-2.5"} pr-1 rounded-full border text-[11px] font-semibold disabled:opacity-60 ${level ? levelChipClass(level) : "border-white/10 text-zinc-500 bg-zinc-950"}`}
        >
          {!level && <option value="">Level…</option>}
          {LEVELS.map((l) => {
            const no = !!blocked(l.id);
            return (
              <option key={l.id} value={l.id} disabled={no} className="bg-zinc-950 text-zinc-200 disabled:text-zinc-500">
                {no ? `${l.label} (${SHORT_WHY[l.id]})` : l.label}
              </option>
            );
          })}
        </select>
        {error && <span role="alert" title={error} dir="auto" className="min-w-0 max-w-[12rem] line-clamp-2 text-[10.5px] leading-tight text-rose-300">{error}</span>}
      </span>
    );
  }

  const reasons = info ? LEVELS.map((l) => whyNot(l.id, info, sessionId)).filter((r): r is string => r !== null) : [];
  return (
    <div className="space-y-1.5">
      <div role="group" aria-label={`Permission level on ${serverName}`} className="flex flex-wrap gap-1.5">
        {LEVELS.map((l) => {
          const on = level === l.id;
          return (
            <button
              key={l.id}
              type="button"
              aria-pressed={on}
              disabled={disabled || !info || !!blocked(l.id)}
              onClick={() => void pick(l.id)}
              className={`h-10 sm:h-8 px-3 rounded-lg text-[11.5px] border flex items-center gap-1.5 disabled:opacity-40 ${
                on ? `${TONE[l.id]} ${l.id === "full" ? "bg-rose-500/15" : "bg-white/5"} font-semibold` : "border-white/10 text-zinc-400 hover:bg-white/5"
              }`}
            >
              {on && isRed(l.id) && <ShieldAlert size={12} className="shrink-0" />}
              {l.label}
            </button>
          );
        })}
      </div>
      {note && <p className="text-[11px] text-zinc-500">{note} A change applies from the assistant's next action.</p>}
      {autoAsAsk && <p className="text-[11px] text-amber-300/90">{autoAsAsk}</p>}
      {reasons.map((r) => <p key={r} className="text-[11px] text-zinc-500">{r}</p>)}
      {error && <p role="alert" dir="auto" className="text-[11px] text-rose-300">{error}</p>}
    </div>
  );
};

interface FullAccessBannerProps {
  serverName: string;
  serverUuid: string;
  sessionId: string;
  info: LevelInfo;
  onChanged: (info: LevelInfo) => void;
}

/** Across the top of the AI tab while Full access is on. */
export const FullAccessBanner = ({ serverName, serverUuid, sessionId, info, onChanged }: FullAccessBannerProps) => {
  const [error, setError] = useState<string | null>(null);
  const busy = useRef(false);
  const alive = useAlive();
  if (!serverUuid || info.level !== "full") return null;

  // Full access is never saved, so the saved level is the one to go back to.
  const back: Level = info.saved === "full" ? "ask" : info.saved;
  const end = async () => {
    if (busy.current) return;
    busy.current = true;
    setError(null);
    try {
      await ai.setLevel(serverUuid, back, { sessionId });
      const fresh = await ai.levelInfo(serverUuid, sessionId);
      if (alive.current) onChanged(fresh);
    } catch (e) {
      if (alive.current) setError(errText(e));
    } finally {
      busy.current = false;
    }
  };

  return (
    <div role="status" className="shrink-0 flex flex-wrap items-center gap-x-2 gap-y-1 px-3 py-1.5 border-b border-rose-500/60 bg-rose-500/25 text-[11.5px] text-rose-100">
      <ShieldAlert size={13} className="shrink-0 text-rose-200" />
      <span className="flex-1 min-w-0">
        <span className="font-bold">Full access on <bdi>{serverName}</bdi>:</span> every command runs without asking. It ends when this tab closes.
      </span>
      {error && <span dir="auto" className="text-[11px] text-rose-200">{error}</span>}
      <button
        type="button"
        onClick={() => void end()}
        title={`Back to ${LEVELS.find((l) => l.id === back)?.label ?? back}`}
        className="shrink-0 h-10 sm:h-7 px-2.5 rounded-md text-[10.5px] font-bold uppercase tracking-wider border border-rose-300/60 bg-rose-500/30 text-rose-50 hover:bg-rose-500/45"
      >
        End now
      </button>
    </div>
  );
};

interface LevelMenuProps {
  serverUuid: string;
  serverName: string;
  /** The level the card shows: "full" while Full access is on in a session. */
  current: Level;
  /** The card's button: the menu opens at it, and the focus goes back to it. */
  anchor: HTMLElement;
  /** Phone sizes. */
  compact: boolean;
  onClose: () => void;
}

/**
 * A server card's level menu. Full access isn't in it: it lives in an open
 * session, so it is turned on in the server's AI tab. Any level picked here
 * ends it.
 */
export const LevelMenu = ({ serverUuid, serverName, current, anchor, compact, onClose }: LevelMenuProps) => {
  const change = useLevelChange();
  const [info, setInfo] = useState<LevelInfo | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [pos, setPos] = useState<{ left: number; top: number } | null>(null);
  const ref = useRef<HTMLDivElement | null>(null);
  const noteId = useId();
  // While a pick asks or saves, nothing closes the menu: an error shows in it.
  const busy = useRef(false);
  const alive = useAlive();

  // The saved level, and whether the server has a POSIX shell. Until that
  // answers, or if it can't, the card's level stands in.
  useEffect(() => {
    let gone = false;
    ai.levelInfo(serverUuid, null).then((i) => { if (!gone) setInfo(i); }).catch(() => {});
    return () => { gone = true; };
  }, [serverUuid]);

  // Under the button, or over it when there is no room below, and inside the
  // window. Measured before it shows, and again on a resize: a phone's
  // keyboard going away is one.
  const place = useCallback(() => {
    const el = ref.current;
    if (!el) return;
    const r = anchor.getBoundingClientRect();
    const { width: w, height: h } = el.getBoundingClientRect();
    const left = Math.max(8, Math.min(r.right - w, window.innerWidth - w - 8));
    const top = r.bottom + 4 + h <= window.innerHeight - 8 ? r.bottom + 4 : Math.max(8, r.top - h - 4);
    setPos({ left, top });
  }, [anchor]);
  useLayoutEffect(place, [place, info, error]);

  // The menu takes the focus, not an item in it: a stray Enter picks nothing.
  useLayoutEffect(() => { ref.current?.focus({ preventScroll: true }); }, []);

  const close = useCallback((refocus: boolean) => {
    if (busy.current) return;
    onClose();
    if (refocus) anchor.focus({ preventScroll: true });
  }, [anchor, onClose]);

  useEffect(() => {
    const onBlur = () => close(false);
    window.addEventListener("resize", place);
    window.addEventListener("blur", onBlur);
    return () => {
      window.removeEventListener("resize", place);
      window.removeEventListener("blur", onBlur);
    };
  }, [place, close]);

  const saved: Level | null = info?.saved ?? (current === "full" ? null : current);

  const pick = async (to: Level) => {
    if (busy.current) return;
    // Its level already: nothing changes, unless Full access is on, which a pick ends.
    if (to === saved && current !== "full") { close(true); return; }
    // Like the item: no Auto without a POSIX shell, unless Auto is the saved level.
    if (to === "auto" && info && noAuto(info) && saved !== "auto") return;
    busy.current = true;
    setError(null);
    let done = false;
    try {
      done = await change({ serverUuid, serverName, from: info ?? (saved ? { level: current, saved } : null), to, sessionId: null });
    } catch (e) {
      if (alive.current) setError(errText(e));
    } finally {
      busy.current = false;
    }
    if (done && alive.current) close(true);
  };

  const onKeyDown = (e: React.KeyboardEvent) => {
    if (e.key === "Escape" || e.key === "Tab") {
      e.preventDefault();
      e.stopPropagation();
      close(true);
      return;
    }
    const items = Array.from(ref.current?.querySelectorAll<HTMLElement>("[role=menuitemradio]:not(:disabled)") ?? []);
    if (items.length === 0) return;
    const i = items.indexOf(document.activeElement as HTMLElement);
    const go = (n: number) => { e.preventDefault(); items[(n + items.length) % items.length].focus(); };
    if (e.key === "ArrowDown") go(i + 1);
    else if (e.key === "ArrowUp") go(i < 0 ? -1 : i - 1);
    else if (e.key === "Home") go(0);
    else if (e.key === "End") go(-1);
  };

  return createPortal(
    <>
      {/* A click outside closes the menu and does nothing else. */}
      <div
        className="fixed inset-0 z-[9996]"
        onClick={() => close(false)}
        onContextMenu={(e) => { e.preventDefault(); close(false); }}
      />
      <div
        ref={ref}
        role="menu"
        aria-label={`AI permission level on ${serverName}`}
        aria-describedby={noteId}
        tabIndex={-1}
        onKeyDown={onKeyDown}
        style={pos ?? { left: -9999, top: -9999 }}
        className="fixed z-[9997] w-60 max-w-[calc(100vw-1rem)] rounded-xl border border-white/10 bg-[#16161a] shadow-2xl shadow-black/60 p-1 text-[12px] outline-none"
      >
        <div className="px-3 pt-1.5 pb-1 text-[10px] font-bold uppercase tracking-wider text-zinc-500">AI permission level</div>
        {LEVELS.filter((l) => l.id !== "full").map((l) => {
          const checked = l.id === saved;
          const no = l.id === "auto" && !!info && noAuto(info) && !checked;
          return (
            <button
              key={l.id}
              type="button"
              role="menuitemradio"
              aria-checked={checked}
              tabIndex={-1}
              disabled={no}
              title={levelNote(l.id, info?.files_only)}
              onClick={() => void pick(l.id)}
              className={`w-full ${compact ? "h-10" : "h-8"} px-2.5 rounded-lg flex items-center gap-2 text-left hover:bg-white/5 focus:bg-white/5 outline-none disabled:opacity-40 disabled:hover:bg-transparent ${l.id === "auto" ? "text-rose-300" : "text-zinc-200"}`}
            >
              <Check size={12} className={`shrink-0 ${checked ? "text-primary" : "invisible"}`} />
              <span className="truncate">{no ? `${l.label} (${SHORT_WHY.auto})` : l.label}</span>
            </button>
          );
        })}
        <div id={noteId} className="mt-1 px-3 pt-1.5 pb-1 border-t border-white/5 space-y-1 text-[11px] leading-snug">
          {current === "full" && <p className="text-rose-300">Full access is on in an open session. Picking a level ends it.</p>}
          <p className="text-zinc-500">Full access is turned on in the server's AI tab.</p>
        </div>
        {error && <p role="alert" dir="auto" className="px-3 pb-1.5 text-[11px] leading-snug text-rose-300">{error}</p>}
      </div>
    </>,
    document.body,
  );
};
