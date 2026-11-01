import { useState, useEffect, useLayoutEffect, useRef, useCallback, memo } from "react";
import { createPortal } from "react-dom";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { TerminalSquare, Folder, FolderUp, FolderSync, Network, AlertTriangle, Check, X, Play, Library, Info, Container, Plus, ChevronDown, SplitSquareHorizontal, Columns, Rows, RotateCw, Loader2, CircleSlash, PanelRight, PanelLeft, PanelBottom, PanelTop, Maximize2, Minimize2, UploadCloud, LogIn } from "lucide-react";
import TerminalView from "./TerminalView";
import { AuthPrompts } from "./AuthPrompts";
import SftpWorkspace, { type SftpView } from "./SftpWorkspace";
import { nextShellFolder, type ShellFolderState } from "../util/shellFolder";
import TunnelsPanel from "./TunnelsPanel";
import InfoPanel from "./InfoPanel";
import { CmdsPanel } from "./CmdsPanel";
import { RenameInput } from "../ui/renameInput";
import { useIsCompact } from "../hooks/useViewport";
import { fontFamilyCss, readFontFamily, readFontSize } from "../util/terminalFont";
import { readShellSyncPairs, useShellSyncServer, type ShellSyncPair } from "../util/shellSync";
import { readUseLocalShell } from "../util/terminalShell";
import { readUseSshAgent } from "../util/sshAgent";
import { useSlideOverFocus } from "../ui/slideOverFocus";
import { useConfirm } from "../ui/confirm";
import { parseObjectList } from "../util/serverJson";

// What the shell-config push commands answer (mirror.rs' ShellSync* types).
type ShellSyncShells = { remote_shell: string; local_shell: string; shell_mismatch: boolean };
type ShellSyncEntry = { local: string; remote: string; size: number; action: string; reason: string | null; crlf: boolean };
type ShellSyncReport = { home: string; entries: ShellSyncEntry[]; total_bytes: number };
type ShellSyncPushResult = { pushed: number; bytes: number; failed: { remote: string; error: string }[]; backup: string | null };

// Where the tool panel (SFTP, Ports, Library, Info) sits around the terminal.
// Remembered per device like the panel's size; compact windows ignore it and
// show the tool full screen.
type ToolDock = "right" | "left" | "bottom" | "top";
const TOOL_DOCK_KEY = "submarine-tool-dock";
const DOCK_OPTIONS: { id: ToolDock; label: string; Icon: typeof PanelRight }[] = [
  { id: "right", label: "Right of the terminal", Icon: PanelRight },
  { id: "left", label: "Left of the terminal", Icon: PanelLeft },
  { id: "bottom", label: "Below the terminal", Icon: PanelBottom },
  { id: "top", label: "Above the terminal", Icon: PanelTop },
];
// The panel is the last element in the DOM; reversing the flex direction
// moves it without remounting the terminals.
const DOCK_FLEX: Record<ToolDock, string> = {
  right: "flex-row",
  left: "flex-row-reverse",
  bottom: "flex-col",
  top: "flex-col-reverse",
};
// Above or below the terminal the panel has a height of its own; the
// terminal always keeps at least MIN_TERMINAL_HEIGHT. Beside it, the same
// with a width: in a narrow split tile a wide panel would otherwise take the
// whole tile and leave the terminal 0 px wide (gone, but still typing).
const MIN_TOOL_HEIGHT = 160;
const MIN_TERMINAL_HEIGHT = 120;
const MIN_TERMINAL_WIDTH = 200;
// The panel can also take the terminal's place ("full screen"); remembered
// with the dock.
const TOOL_FULL_KEY = "submarine-tool-full";

// Title-bar button of a tool panel: give the panel the terminal's room, or
// bring the terminal back. The terminal keeps running either way.
const FullToggle = ({ full, onToggle }: { full: boolean; onToggle: () => void }) => (
  <button
    onClick={onToggle}
    aria-pressed={full}
    aria-label="Full screen"
    title={full ? "Show the terminal again" : "Full screen: hide the terminal and give this panel all the room"}
    className={`shrink-0 transition-colors ${full ? "text-primary hover:text-white" : "text-zinc-500 hover:text-white"}`}
  >
    {full ? <Minimize2 size={13} /> : <Maximize2 size={13} />}
  </button>
);

// Compact "run this tab on its own dedicated SSH connection" toggle, shown in
// the SFTP and Port-Forwarding tab headers. The status dot reflects the live
// state of the dedicated connection: green = up, amber (pulsing) = opening /
// applies-on-connect, red = couldn't be established (falling back to the main
// session). No dot when the toggle is off.
//
// The label is one word so it fits a narrow pane. Hovering it, or moving
// keyboard focus to it, shows a short guide: what the option does (`hint`),
// when the terminal's connection is used anyway, and what the dot's colours
// mean.
const SEP_GUIDE_WIDTH = 264;
const SEP_GUIDE_HEIGHT = 136; // enough for the longest hint; only used to pick a side
const SepToggle = ({ on, onToggle, status, hint, onReconnect }: {
  on: boolean;
  onToggle: (v: boolean) => void;
  status: 'ready' | 'pending' | 'failed' | 'off';
  hint: string;
  onReconnect?: () => void;
}) => {
  // The label's box while its guide shows; null while it is hidden.
  const [guideAt, setGuideAt] = useState<{ left: number; top: number; bottom: number } | null>(null);
  const showGuide = (label: HTMLElement) => {
    const r = label.getBoundingClientRect();
    setGuideAt({ left: r.left, top: r.top, bottom: r.bottom });
  };
  return (
    <span className="flex items-center gap-1.5">
      <label
        className="flex items-center gap-1.5 text-[10px] text-zinc-400 hover:text-zinc-200 cursor-pointer select-none"
        // Touch screens fake a hover on tap, which would leave the guide up
        // after every toggle, so only a real pointer opens it.
        onMouseEnter={(e) => { if (window.matchMedia('(hover: hover)').matches) showGuide(e.currentTarget); }}
        onMouseLeave={() => setGuideAt(null)}
        // Keyboard focus only: a mouse click focuses the checkbox too, and
        // the guide would then outlive the pointer leaving.
        onFocus={(e) => { if (e.target.matches(':focus-visible')) showGuide(e.currentTarget); }}
        onBlur={() => setGuideAt(null)}
      >
        <input type="checkbox" className="w-3 h-3 accent-primary" checked={on} aria-label="Dedicated session" onChange={(e) => onToggle(e.target.checked)} />
        <span className="uppercase tracking-wider font-bold">Dedicated</span>
        {on && (
          <span
            className={`w-1.5 h-1.5 rounded-full shrink-0 ${
              status === 'ready' ? 'bg-emerald-400' : status === 'failed' ? 'bg-red-400' : 'bg-amber-400 animate-pulse-brief'
            }`}
          />
        )}
      </label>
      {/* Through a portal: the pane's title bar clips what overflows it.
          Below the label, or above it when there is no room below. */}
      {guideAt && createPortal(
        <div
          role="tooltip"
          style={{
            width: SEP_GUIDE_WIDTH,
            left: Math.max(8, Math.min(guideAt.left, window.innerWidth - SEP_GUIDE_WIDTH - 8)),
            ...(guideAt.bottom + 6 + SEP_GUIDE_HEIGHT <= window.innerHeight
              ? { top: guideAt.bottom + 6 }
              : { bottom: window.innerHeight - guideAt.top + 6 }),
          }}
          className="fixed z-[9999] pointer-events-none bg-[#15151a] border border-white/10 rounded-lg shadow-2xl px-3 py-2 text-[11px] leading-snug text-zinc-300"
        >
          <div className="mb-1 text-[10px] font-bold uppercase tracking-wider text-zinc-100">Dedicated session</div>
          <p>{hint}</p>
          <p className="mt-1 text-zinc-400">When it is off, or that connection can't be opened, the terminal's connection is used.</p>
          <p className="mt-1.5 flex flex-wrap items-center gap-x-3 gap-y-0.5 text-[10px] text-zinc-400">
            <span className="flex items-center gap-1"><span className="w-1.5 h-1.5 rounded-full bg-emerald-400" />connected</span>
            <span className="flex items-center gap-1"><span className="w-1.5 h-1.5 rounded-full bg-amber-400" />connecting</span>
            <span className="flex items-center gap-1"><span className="w-1.5 h-1.5 rounded-full bg-red-400" />failed</span>
          </p>
        </div>,
        document.body
      )}
      {/* Reconnect just the dedicated connection — outside the <label> so a
          click can't toggle the checkbox. Shown whenever the toggle is on and
          no connect attempt is already in flight; also the retry affordance
          after a red (failed) dot. */}
      {on && onReconnect && status !== 'pending' && (
        <button
          onClick={onReconnect}
          title="Reconnect the dedicated session"
          className="p-0.5 rounded text-zinc-500 hover:text-white hover:bg-white/10 transition-colors"
        >
          <RotateCw size={10} />
        </button>
      )}
    </span>
  );
};

// The seconds left until `until` (a Date.now() time), ticking on its own: a
// reconnect countdown re-renders this number, not the whole session view
// twice a second, for as long as a server stays down.
const SecondsLeft = ({ until }: { until: number }) => {
  const left = () => Math.max(0, Math.ceil((until - Date.now()) / 1000));
  const [secs, setSecs] = useState(left);
  useEffect(() => {
    setSecs(left());
    const t = setInterval(() => setSecs(left()), 250);
    return () => clearInterval(t);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [until]);
  return <>{secs}</>;
};

// The title bar of a tool panel with a Dedicated toggle (SFTP, Port
// Forwarding): the title, the toggle, then the panel's buttons (SFTP's Follow
// and Mirror), full screen and close. In a narrow panel not all of it fits.
// What gives way, in this order: the title shortens ("SFTP", "Ports"), the
// title goes, and only then do Follow and Mirror lose their words. So the
// words are there whenever there is room for them. The bar measures itself
// instead of using fixed widths, because the toggle is wider while its status
// dot shows and the text's width depends on the font.
const TITLE_BAR_GAP = 12; // gap-3 between the title, the toggle and the buttons
const WORD_GAP = 6; // gap-1.5 between a button's icon and its word
const TITLE_BAR_TEXT = "text-[10px] font-bold uppercase tracking-wider";
interface TitleBarFit { title: "full" | "short" | "none"; words: boolean }
const TITLE_BAR_FITS: TitleBarFit[] = [
  { title: "full", words: true },
  { title: "short", words: true },
  { title: "none", words: true },
  { title: "short", words: false },
  { title: "none", words: false },
];
const ToolTitleBar = ({ title, shortTitle, toggle, follow, mirror, extra, onClose }: {
  title: string;
  shortTitle: string;
  toggle: React.ReactNode;
  /** SFTP's Follow button (the remote pane follows the terminal's folder); the other panels have none. */
  follow?: { on: boolean; onToggle: () => void };
  /** SFTP's Mirror button; the other panels have none. */
  mirror?: { on: boolean; disabled: boolean; onToggle: () => void };
  /** Buttons between Mirror and Close (full screen). */
  extra: React.ReactNode;
  onClose: () => void;
}) => {
  const barRef = useRef<HTMLDivElement | null>(null);
  const toggleRef = useRef<HTMLSpanElement | null>(null);
  const buttonsRef = useRef<HTMLDivElement | null>(null);
  // Never shown: they only give the texts' widths.
  const fullTitleRef = useRef<HTMLSpanElement | null>(null);
  const shortTitleRef = useRef<HTMLSpanElement | null>(null);
  const followWordRef = useRef<HTMLSpanElement | null>(null);
  const mirrorWordRef = useRef<HTMLSpanElement | null>(null);
  const [fitIndex, setFitIndex] = useState(0);
  const fit = TITLE_BAR_FITS[fitIndex];
  const fitRef = useRef(fit);
  fitRef.current = fit;
  useLayoutEffect(() => {
    const bar = barRef.current, toggleBox = toggleRef.current, buttons = buttonsRef.current;
    if (!bar || !toggleBox || !buttons || typeof ResizeObserver === "undefined") return;
    const width = (el: HTMLElement | null) => (el ? el.getBoundingClientRect().width : 0);
    const measure = () => {
      const style = getComputedStyle(bar);
      const room = bar.clientWidth - parseFloat(style.paddingLeft) - parseFloat(style.paddingRight);
      if (!(room > 0)) return; // not laid out
      const wordOf = (el: HTMLElement | null) => (el ? width(el) + WORD_GAP : 0);
      const word = wordOf(followWordRef.current) + wordOf(mirrorWordRef.current);
      const iconButtons = width(buttons) - (fitRef.current.words ? word : 0);
      const titles = { full: width(fullTitleRef.current), short: width(shortTitleRef.current), none: 0 };
      const needed = (f: TitleBarFit) =>
        (f.title === "none" ? 0 : titles[f.title] + TITLE_BAR_GAP)
        + width(toggleBox) + TITLE_BAR_GAP + iconButtons + (f.words ? word : 0);
      // A pixel to spare, so rounding never leaves the last one a hair too wide.
      const first = TITLE_BAR_FITS.findIndex((f) => needed(f) + 1 <= room);
      setFitIndex(first === -1 ? TITLE_BAR_FITS.length - 1 : first);
    };
    const observer = new ResizeObserver(measure);
    observer.observe(bar);
    observer.observe(toggleBox);
    observer.observe(buttons);
    measure();
    return () => observer.disconnect();
  }, []);
  return (
    <div ref={barRef} className="relative h-10 px-4 flex items-center gap-3 border-b border-white/5 bg-white/5 overflow-hidden">
      {fit.title !== "none" && (
        <span className={`${TITLE_BAR_TEXT} text-zinc-400 truncate min-w-0`}>
          {fit.title === "full" ? title : shortTitle}
        </span>
      )}
      <span ref={toggleRef} className="shrink-0 whitespace-nowrap">{toggle}</span>
      <div ref={buttonsRef} className="ml-auto flex items-center gap-2 shrink-0">
        {follow && (
          <button
            onClick={follow.onToggle}
            aria-pressed={follow.on}
            aria-label="Follow terminal folder"
            title={follow.on
              ? "Following the terminal: the remote files go to the folder the terminal is in. Click to stop."
              : "Follow the terminal: the remote files go to the folder the terminal is in"}
            className={`h-6 px-2 rounded border flex items-center gap-1.5 ${TITLE_BAR_TEXT} transition-all ${
              follow.on
                ? 'text-primary bg-primary/10 border-primary/30'
                : 'text-zinc-400 border-white/10 hover:text-white hover:bg-white/5'
            }`}
          >
            <FolderSync size={12} className="shrink-0" />
            {fit.words && <span>Follow</span>}
          </button>
        )}
        {mirror && (
          <button
            onClick={mirror.onToggle}
            disabled={mirror.disabled}
            aria-pressed={mirror.on}
            aria-label="Mirror"
            title={mirror.disabled
              ? "Mirror needs a saved server"
              : mirror.on ? "Back to the files" : "Mirror: keep a local folder copied to this server"}
            className={`h-6 px-2 rounded border flex items-center gap-1.5 ${TITLE_BAR_TEXT} transition-all disabled:opacity-40 disabled:cursor-not-allowed ${
              mirror.on
                ? 'text-primary bg-primary/10 border-primary/30'
                : 'text-zinc-400 border-white/10 hover:text-white hover:bg-white/5'
            }`}
          >
            <FolderUp size={12} className="shrink-0" />
            {fit.words && <span>Mirror</span>}
          </button>
        )}
        {extra}
        <button onClick={onClose} aria-label="Close" className="text-zinc-500 hover:text-white transition-colors shrink-0">
          <X size={14} />
        </button>
      </div>
      <div aria-hidden className="absolute left-0 top-0 invisible pointer-events-none whitespace-nowrap flex gap-4">
        <span ref={fullTitleRef} className={TITLE_BAR_TEXT}>{title}</span>
        <span ref={shortTitleRef} className={TITLE_BAR_TEXT}>{shortTitle}</span>
        {follow && <span ref={followWordRef} className={TITLE_BAR_TEXT}>Follow</span>}
        {mirror && <span ref={mirrorWordRef} className={TITLE_BAR_TEXT}>Mirror</span>}
      </div>
    </div>
  );
};

// The name of a new terminal tab: the lowest number no open terminal has
// (issue #231). Closing "2" of 1, 2, 3 and opening another gives a new "2",
// never a second "3". A renamed terminal keeps its number (`baseTitle`), and
// the number it shows now is taken too (renaming "2" to "3" leaves no second
// "3" to make); a container's terminal, named after the container, takes none.
function nextTerminalNumber(terminals: { title: string; baseTitle?: string; container?: unknown }[]): string {
  const used = new Set(
    terminals
      .filter((t) => !t.container)
      .flatMap((t) => [t.title, t.baseTitle ?? t.title])
      .filter((t) => /^\d+$/.test(t))
      .map(Number),
  );
  let n = 1;
  while (used.has(n)) n++;
  return `${n}`;
}

// A server's on-connect commands as they are typed: a line each, blank
// lines left out, each sent with Enter.
function onConnectText(commands: string): string {
  return commands
    .split(/\r?\n/)
    .map((l) => l.trimEnd())
    .filter((l) => l.trim().length > 0)
    .join('\n') + '\n';
}

// Servers whose new terminals log in on their own (issue #230): this device's
// choice, by the server's sync id, never synced.
const OWN_LOGIN_KEY = "submarine-terminal-own-login";
function readOwnLogin(uuid?: string | null): boolean {
  if (!uuid) return false;
  try {
    return JSON.parse(localStorage.getItem(OWN_LOGIN_KEY) || "{}")[uuid] === true;
  } catch {
    return false;
  }
}
function writeOwnLogin(uuid: string, on: boolean) {
  try {
    const all = JSON.parse(localStorage.getItem(OWN_LOGIN_KEY) || "{}");
    if (on) all[uuid] = true;
    else delete all[uuid];
    localStorage.setItem(OWN_LOGIN_KEY, JSON.stringify(all));
  } catch {
    // Best-effort, like the other per-device choices.
  }
}

// SFTP's Follow (#245): the remote pane goes to the folder the active
// terminal's shell reports. Off until turned on; this device's choice, for
// every server tab, which hear of a change through FOLLOW_EVENT.
const FOLLOW_KEY = "submarine-sftp-follow-terminal";
const FOLLOW_EVENT = "submarine-sftp-follow-changed";
function readFollow(): boolean {
  try {
    return localStorage.getItem(FOLLOW_KEY) === "on";
  } catch {
    return false;
  }
}
function writeFollow(on: boolean) {
  try {
    localStorage.setItem(FOLLOW_KEY, on ? "on" : "off");
  } catch {
    // Best-effort: the open tabs still hear of it.
  }
  window.dispatchEvent(new CustomEvent<boolean>(FOLLOW_EVENT, { detail: on }));
}

// `displayName`: the session tab's name, which the user may have changed
// from the server's (DesktopApp's tabNames). `profile`: the unlocked
// profile's name, for what is kept per server on this device.
const SessionViewImpl = ({ session, profile, displayName, onClose, addLog, onStatusChange, chromeless = false, onTerminalsChange, isActiveView = false, onPromptChange }: any) => {
  const [status, setStatus] = useState<'connecting' | 'connected' | 'failed' | 'disconnected'>('connecting');

  // Bubble every status change up to the parent so the session-tab strip
  // can draw a coloured dot (green / amber / red) next to the name without
  // having to mount its own listeners or duplicate the connection-event
  // wiring we already do here.
  useEffect(() => {
    if (typeof onStatusChange === "function") {
      onStatusChange(session?.id, status);
    }
  }, [status, session?.id, onStatusChange]);
  const [disconnectReason, setDisconnectReason] = useState<string>("");
  const [logs, setLogs] = useState<{ msg: string, type: string, time?: string }[]>([]);
  // Cap to keep a chatty session (monitors, mirrors, MOTD spam) from
  // ballooning React state — each render copies the whole array, so an
  // unbounded log makes the UI quadratically slower over time.
  const LOG_CAP = 500;
  const pushLog = (entry: { msg: string, type: string }) => {
    const stamped = { ...entry, time: new Date().toLocaleTimeString() };
    setLogs(prev => prev.length >= LOG_CAP
      ? [...prev.slice(prev.length - LOG_CAP + 1), stamped]
      : [...prev, stamped]);
  };
  // The connect-time log box follows new lines — kept scrolled to the bottom —
  // while the user is at the bottom. Scrolling up to read pauses that until
  // they scroll back down; a new attempt (logs cleared) follows again. Without
  // it the latest line, i.e. what the connection is doing right now, sat below
  // the fold on a short window. Layout effect so it lands before paint.
  const logBoxRef = useRef<HTMLDivElement | null>(null);
  const logFollowRef = useRef(true);
  useLayoutEffect(() => {
    if (logs.length === 0) logFollowRef.current = true;
    const el = logBoxRef.current;
    if (el && logFollowRef.current) el.scrollTop = el.scrollHeight;
  }, [logs]);
  // The box only exists on the connect screen. Whenever it (re)appears —
  // e.g. after the reconnect banner was cancelled, with lines already in it —
  // start at the newest line and follow again.
  const attachLogBox = useCallback((el: HTMLDivElement | null) => {
    logBoxRef.current = el;
    if (el) {
      logFollowRef.current = true;
      el.scrollTop = el.scrollHeight;
    }
  }, []);
  const [fingerprintPrompt, setFingerprintPrompt] = useState<any>(null);
  // Keyboard-interactive (2FA / verification-code) prompt. `kbiPrompt` holds
  // the backend payload ({ nonce, name, instructions, prompts:[{prompt,echo}] });
  // `kbiValues` mirrors one editable answer per prompt.
  const [kbiPrompt, setKbiPrompt] = useState<any>(null);
  const [kbiValues, setKbiValues] = useState<string[]>([]);
  // Nonce of the prompt on screen, read synchronously by the dismiss
  // listeners — state would lag a render behind a dismiss that races in.
  const fpNonceRef = useRef<string | null>(null);
  const kbiNonceRef = useRef<string | null>(null);
  // The prompt's answer field takes the caret only while this session is on
  // screen: when the prompt appears with the tab in front, or when the user
  // switches to a tab whose prompt is waiting. A hidden tab's prompt must not
  // swallow what's typed into the visible one.
  const kbiFirstInputRef = useRef<HTMLInputElement | null>(null);
  useEffect(() => {
    if (kbiPrompt && isActiveView) kbiFirstInputRef.current?.focus();
  }, [kbiPrompt, isActiveView]);
  // Tell the tab strip while a prompt waits on the user — a background tab's
  // prompt is hidden along with its tab, so the tab is marked instead.
  const promptWaiting = !!(fingerprintPrompt || kbiPrompt);
  useEffect(() => {
    onPromptChange?.(session.id, promptWaiting);
  }, [onPromptChange, session.id, promptWaiting]);
  useEffect(() => () => onPromptChange?.(session.id, false), [onPromptChange, session.id]);
  const [isAuthError, setIsAuthError] = useState(false);
  const [customPassword, setCustomPassword] = useState("");
  // The connect-time log box mirrors the terminal font-size setting, so the
  // user's chosen size applies to the startup logs too — not just the shell.
  // `submarine-settings-changed` (dispatched by Settings on save) keeps it live.
  const [logFontSize, setLogFontSize] = useState(readFontSize);
  const [logFontFamily, setLogFontFamily] = useState(() => fontFamilyCss(readFontFamily()));
  useEffect(() => {
    const sync = () => {
      setLogFontSize(readFontSize());
      setLogFontFamily(fontFamilyCss(readFontFamily()));
    };
    window.addEventListener('submarine-settings-changed', sync);
    return () => window.removeEventListener('submarine-settings-changed', sync);
  }, []);
  // The connection effect runs once and captures `customPassword=""` in its
  // closure. Auto-reconnect attempts and the disconnect listener that
  // schedules them must read the LATEST password the user typed into the
  // auth-error retry input — so we mirror it into a ref.
  const customPasswordRef = useRef("");
  useEffect(() => { customPasswordRef.current = customPassword; }, [customPassword]);

  // Mirror `status` into a ref so the one-shot effect below (which registers
  // its listeners from its INITIAL closure) can see the latest value — a
  // snapshot captured at effect-run time is always `'connecting'`.
  const statusRef = useRef(status);
  useEffect(() => { statusRef.current = status; }, [status]);

  // ---- Dedicated SSH connections for SFTP / port-forwarding -----------------
  // Two INDEPENDENT, per-tab toggles (the controls live in the SFTP and
  // Port-Forwarding tab headers, persisted globally in localStorage under
  // `submarine-separate-sftp` / `submarine-separate-fwd`, default OFF). When a
  // toggle is on we open a dedicated secondary SSH connection — keyed
  // `${session.id}::sftp` / `${session.id}::fwd` — as a pure TRANSPORT: the
  // backend routes SFTP subsystems and tunnels over it when it exists, while
  // everything stays keyed/tagged under the plain session id. The panels never
  // re-key, tunnels migrate live in both directions, and if a dedicated
  // connection can't be established (2FA server, network blip) everything just
  // rides the primary. Unexpected drops auto-retry with backoff, and each tab
  // header has a manual reconnect button for the dedicated connection.
  type SecondaryStatus = 'off' | 'connecting' | 'ready' | 'failed';
  const [separateSftp, setSeparateSftp] = useState(localStorage.getItem('submarine-separate-sftp') === 'true');
  const [separateFwd, setSeparateFwd] = useState(localStorage.getItem('submarine-separate-fwd') === 'true');
  const separateSftpRef = useRef(separateSftp);
  const separateFwdRef = useRef(separateFwd);
  useEffect(() => { separateSftpRef.current = separateSftp; }, [separateSftp]);
  useEffect(() => { separateFwdRef.current = separateFwd; }, [separateFwd]);

  // Live status of the dedicated connections — drives the tab-header dots and
  // the retry logic only; nothing routes on it (the backend picks transports).
  const [sftpConnStatus, setSftpConnStatus] = useState<SecondaryStatus>('off');
  const [fwdConnStatus, setFwdConnStatus] = useState<SecondaryStatus>('off');
  const sftpConnStatusRef = useRef<SecondaryStatus>('off');
  const fwdConnStatusRef = useRef<SecondaryStatus>('off');
  useEffect(() => { sftpConnStatusRef.current = sftpConnStatus; }, [sftpConnStatus]);
  useEffect(() => { fwdConnStatusRef.current = fwdConnStatus; }, [fwdConnStatus]);

  // Bounded auto-retry for unexpected secondary drops: 2s → 5s → 10s, then
  // give up (red dot; manual reconnect button still works). Counters reset on
  // success and on any manual action.
  const SECONDARY_RETRY_DELAYS = [2000, 5000, 10000];
  const sftpRetryRef = useRef(0);
  const fwdRetryRef = useRef(0);
  const sftpRetryTimerRef = useRef<ReturnType<typeof setTimeout> | null>(null);
  const fwdRetryTimerRef = useRef<ReturnType<typeof setTimeout> | null>(null);
  const clearSecondaryRetryTimers = () => {
    if (sftpRetryTimerRef.current) { clearTimeout(sftpRetryTimerRef.current); sftpRetryTimerRef.current = null; }
    if (fwdRetryTimerRef.current) { clearTimeout(fwdRetryTimerRef.current); fwdRetryTimerRef.current = null; }
  };

  const openSftpConn = () => {
    setSftpConnStatus('connecting');
    invoke("initiate_connection", {
      sessionId: `${session.id}::sftp`,
      serverId: session.serverId,
      customPassword: customPasswordRef.current || null,
      quickAuth: session.quickAuth || null,
      sessionRole: "sftp",
      separateSessions: false,
      useSshAgent: readUseSshAgent(),
    }).catch(console.error);
  };
  const openFwdConn = () => {
    setFwdConnStatus('connecting');
    invoke("initiate_connection", {
      sessionId: `${session.id}::fwd`,
      serverId: session.serverId,
      customPassword: customPasswordRef.current || null,
      quickAuth: session.quickAuth || null,
      sessionRole: "forward",
      separateSessions: false,
      useSshAgent: readUseSshAgent(),
    }).catch(console.error);
  };

  // Restore forwarding on the best available transport (primary, unless a
  // dedicated connection is up). Called when the dedicated path is abandoned —
  // toggle-off, or retries exhausted — so tunnels always land somewhere.
  const restoreTunnelsOnPrimary = () => {
    invoke("restart_session_tunnels", { sessionId: session.id, serverId: session.serverId }).catch(console.error);
  };

  // Whether the in-flight PRIMARY connect deferred its saved tunnels for a
  // `::fwd` connection (i.e. separateFwd was on when we sent it). If the user
  // toggles fwd OFF during the primary handshake, the primary won't have
  // started the tunnels AND openSecondaryConnections won't open `::fwd`, so
  // they'd start nowhere — the success handler reconciles using this.
  const primarySeparateRef = useRef(false);

  // Schedule an auto-retry for a dropped secondary. Fires only if the toggle
  // is still on and the primary is still connected at fire time.
  const scheduleSecondaryRetry = (which: 'sftp' | 'fwd') => {
    const retryRef = which === 'sftp' ? sftpRetryRef : fwdRetryRef;
    const timerRef = which === 'sftp' ? sftpRetryTimerRef : fwdRetryTimerRef;
    const enabledRef = which === 'sftp' ? separateSftpRef : separateFwdRef;
    const setStatus = which === 'sftp' ? setSftpConnStatus : setFwdConnStatus;
    if (retryRef.current >= SECONDARY_RETRY_DELAYS.length) {
      // Out of retries — mark failed; operations ride the primary. For
      // forwarding, explicitly restore the tunnels there.
      setStatus('failed');
      if (which === 'fwd') restoreTunnelsOnPrimary();
      return;
    }
    const delay = SECONDARY_RETRY_DELAYS[retryRef.current];
    retryRef.current += 1;
    setStatus('connecting');
    if (timerRef.current) clearTimeout(timerRef.current);
    timerRef.current = setTimeout(() => {
      timerRef.current = null;
      if (!enabledRef.current || statusRef.current !== 'connected') return;
      if (which === 'sftp') openSftpConn(); else openFwdConn();
    }, delay);
  };

  // Open whichever dedicated connections are enabled — called from the primary
  // `connection-success` handler (first-connect AND reconnect; the backend
  // sweeps stale secondaries at the top of the primary's connect, so this is
  // clean). On `::fwd` success the backend migrates the session's tunnels onto
  // the fresh transport.
  const openSecondaryConnections = () => {
    if (separateSftpRef.current) openSftpConn();
    if (separateFwdRef.current) openFwdConn();
  };

  // Reset secondary tracking when the primary drops / reconnects. The backend
  // has already torn the secondaries down (primary connect-teardown, or the
  // primary disconnect sweep); this just resets the indicators + retry state.
  const resetSecondaryStatus = () => {
    clearSecondaryRetryTimers();
    sftpRetryRef.current = 0;
    fwdRetryRef.current = 0;
    setSftpConnStatus('off');
    setFwdConnStatus('off');
  };

  // Manual reconnect buttons in the tab headers. initiate_connection sweeps
  // its own key's stale state, so a plain re-open is a full clean reconnect
  // of just the dedicated connection — never the whole server session.
  const reconnectSftpConn = () => {
    if (statusRef.current !== 'connected') return;
    sftpRetryRef.current = 0;
    openSftpConn();
  };
  const reconnectFwdConn = () => {
    if (statusRef.current !== 'connected') return;
    fwdRetryRef.current = 0;
    openFwdConn();
  };

  // Tab-header toggles. Persist immediately; apply live if the session is
  // already connected, otherwise they take effect on the next connect.
  const toggleSeparateSftp = (on: boolean) => {
    setSeparateSftp(on);
    // Sync ref update: the disconnect below emits session-disconnected-…::sftp,
    // and its listener consults this ref — the useEffect mirror only lands
    // after the next render, which can be AFTER the event. Without this, a
    // toggle-off could read a stale "on" and auto-retry the connection the
    // user just closed.
    separateSftpRef.current = on;
    localStorage.setItem('submarine-separate-sftp', String(on));
    sftpRetryRef.current = 0;
    if (statusRef.current !== 'connected') return;
    if (on) {
      if (sftpConnStatusRef.current !== 'ready' && sftpConnStatusRef.current !== 'connecting') openSftpConn();
    } else {
      setSftpConnStatus('off');
      // Backend drops the base SFTP cache with the transport, so the next
      // file operation transparently re-opens on the primary.
      invoke("disconnect_session", { sessionId: `${session.id}::sftp` }).catch(console.error);
    }
  };
  const toggleSeparateFwd = (on: boolean) => {
    setSeparateFwd(on);
    // Sync ref update — same stale-ref race as toggleSeparateSftp.
    separateFwdRef.current = on;
    localStorage.setItem('submarine-separate-fwd', String(on));
    fwdRetryRef.current = 0;
    if (statusRef.current !== 'connected') return;
    if (on) {
      if (fwdConnStatusRef.current !== 'ready' && fwdConnStatusRef.current !== 'connecting') openFwdConn();
    } else {
      setFwdConnStatus('off');
      // Order matters: the disconnect stops tunnels riding the dedicated
      // transport AND removes it from the connections map; only then does
      // the restart re-resolve the transport — now the primary — and bring
      // the same tunnels back up there.
      invoke("disconnect_session", { sessionId: `${session.id}::fwd` })
        .catch(console.error)
        .finally(() => { restoreTunnelsOnPrimary(); });
    }
  };

  // Terminal IDs MUST be unique across every open session, not just within
  // this one — the backend dispatches `terminal-output-<id>` events globally
  // and any collision means two tabs end up reading from the same PTY.
  // Scoping the id by `session.id` ensures Server A's "term-0" never clashes
  // with Server B's "term-0".
  // A terminal slot is either a regular login shell (the default) or a
  // docker-exec session into a specific container (spawned from the Info
  // panel). Container slots stay otherwise identical — same xterm, same
  // events — they just point the backend at `open_container_terminal`.
  // `baseTitle`: the title a tab had before it was renamed, which an empty
  // name brings back. `ownLogin`: the terminal logs in on a connection of
  // its own (TerminalView), not on the tab's.
  const [terminals, setTerminals] = useState<{id: string, title: string, baseTitle?: string, container?: { name: string; useSudo: boolean }, ownLogin?: boolean}[]>(() => {
    return [{ id: `${session.id}-term-0`, title: '1' }];
  });
  // Bumped on every successful reconnect. TerminalView watches this prop
  // and re-opens its PTY on change WITHOUT disposing its xterm instance,
  // so the user keeps the scroll-back from before the drop. Without this
  // we used to mint a brand-new terminal_id and remount TerminalView,
  // which dropped the entire visual buffer of their previous session.
  const [connectionEpoch, setConnectionEpoch] = useState(0);
  const openContainerTerminal = (containerName: string, useSudo: boolean) => {
    const newId = `${session.id}-term-${Date.now()}`;
    setTerminals(prev => [...prev, {
      id: newId,
      title: containerName.length > 10 ? `${containerName.slice(0, 9)}…` : containerName,
      container: { name: containerName, useSudo },
    }]);
    setActiveTab(newId);
    setActiveTool(null);
  };

  const [activeTab, setActiveTab] = useState<string>(`${session.id}-term-0`);
  // New terminals of this server log in on their own (its choice on this device).
  const [ownLoginDefault, setOwnLoginDefault] = useState(() => readOwnLogin(session.serverUuid));
  // SFTP's Follow, and the folder each terminal's shell last reported (a
  // container's terminal reports none: its folders aren't the server's).
  const [follow, setFollow] = useState(readFollow);
  const followRef = useRef(follow);
  followRef.current = follow;
  useEffect(() => {
    const sync = (e: Event) => setFollow(!!(e as CustomEvent<boolean>).detail);
    window.addEventListener(FOLLOW_EVENT, sync);
    return () => window.removeEventListener(FOLLOW_EVENT, sync);
  }, []);
  const termFoldersRef = useRef(new Map<string, ShellFolderState>());
  const [, setFolderTick] = useState(0);
  const onTerminalFolder = useCallback((terminalId: string, path: string, host: string) => {
    const next = nextShellFolder(termFoldersRef.current.get(terminalId), path, host);
    if (!next) return;
    termFoldersRef.current.set(terminalId, next);
    if (followRef.current) setFolderTick((n) => n + 1);
  }, []);
  // What the SFTP panel follows: the active terminal's folder, null while its
  // shell hasn't said, undefined while Follow is off.
  const followFolder = follow ? (termFoldersRef.current.get(activeTab) ?? null) : undefined;
  // Bubble our terminals + active-tab up to the parent (App) whenever
  // they change so the Wall pinboard and any other App-level consumers
  // don't have to duplicate the per-session terminal book-keeping.
  // Attach-only Wall tiles use these to know which (sessionId, terminalId)
  // pairs exist and which one is currently the session's own primary.
  useEffect(() => {
    if (typeof onTerminalsChange === "function") {
      onTerminalsChange(session.id, terminals, activeTab);
    }
  }, [session?.id, terminals, activeTab, onTerminalsChange]);

  // ── Focus the terminal when its tab (or this session's server tab) is
  // selected ── issue #51. We don't reach into xterm from here; we bump a
  // monotonic signal that TerminalView watches, and the terminal that is
  // currently active focuses itself (with its own touch / input / dialog
  // guards). `requestTerminalFocus` is the single entry point so the
  // "don't fight an open auth prompt" rule lives in one place.
  const [focusTick, setFocusTick] = useState(0);
  const requestTerminalFocus = () => {
    // A fingerprint / 2FA prompt owns the keyboard while it's up — don't
    // yank focus out from under the user answering it. (TerminalView also
    // refuses focus while disabled, which it is during these prompts.)
    if (fingerprintPrompt || kbiPrompt) return;
    setFocusTick(t => t + 1);
  };
  // When this SessionView becomes the frontmost server tab, pull focus into
  // its active terminal so switching servers lets you type right away.
  const prevActiveViewRef = useRef(isActiveView);
  useEffect(() => {
    const was = prevActiveViewRef.current;
    prevActiveViewRef.current = isActiveView;
    if (isActiveView && !was) requestTerminalFocus();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [isActiveView]);
  // Connected again: the terminal refused the focus while it was down, and
  // the Reconnect button that had it is gone. The terminal keeps its guards:
  // a tab behind others, or a field in use, keeps the focus where it is.
  useEffect(() => {
    if (status === 'connected') requestTerminalFocus();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [status]);

  const [activeTool, setActiveTool] = useState<'sftp' | 'tunnels' | 'mirrors' | 'cmds' | 'info' | null>(null);

  // "Push shell config": a one-shot push, not a dockable tool pane like the
  // four above, so it gets its own preview/confirm dialog. Offered on the
  // servers picked in Settings → Shell config sync (this device's choice).
  const shellSyncOffered = useShellSyncServer(session.serverUuid);
  const [shellSyncOpen, setShellSyncOpen] = useState(false);
  const [shellSyncBusy, setShellSyncBusy] = useState(false);
  const [shellSyncError, setShellSyncError] = useState<string | null>(null);
  // The pairs the preview was made for: a push sends exactly those.
  const [shellSyncPairs, setShellSyncPairs] = useState<ShellSyncPair[]>([]);
  const [shellSyncShells, setShellSyncShells] = useState<ShellSyncShells | null>(null);
  const [shellSyncReport, setShellSyncReport] = useState<ShellSyncReport | null>(null);
  // A push with failures stays open to list them.
  const [shellSyncResult, setShellSyncResult] = useState<ShellSyncPushResult | null>(null);
  // Bumped on each opening, so that an answer meant for an earlier one is
  // dropped.
  const shellSyncRun = useRef(0);
  const shellSyncPanelRef = useSlideOverFocus(shellSyncOpen);

  const fmtBytes = (n: number) => {
    if (n < 1024) return `${n} B`;
    if (n < 1024 * 1024) return `${(n / 1024).toFixed(1)} KB`;
    return `${(n / 1024 / 1024).toFixed(1)} MB`;
  };

  const openShellSync = async () => {
    const run = ++shellSyncRun.current;
    const pairs = readShellSyncPairs();
    setShellSyncOpen(true);
    setShellSyncError(null);
    setShellSyncReport(null);
    setShellSyncResult(null);
    setShellSyncShells(null);
    setShellSyncPairs(pairs);
    if (pairs.length === 0) {
      setShellSyncError("Nothing to push yet: add files in Settings → Shell config sync.");
      return;
    }
    setShellSyncBusy(true);
    // Which shell the account logs into, for a note. Not asked when new
    // terminals start this computer's own shell anyway (Settings → Use my
    // default shell), and never in the way of the preview.
    if (!readUseLocalShell()) {
      invoke<ShellSyncShells>("shell_sync_probe", { sessionId: session.id })
        .then((s) => { if (run === shellSyncRun.current) setShellSyncShells(s); }, () => {});
    }
    try {
      const report = await invoke<ShellSyncReport>("shell_sync_preview", { sessionId: session.id, pairs });
      if (run === shellSyncRun.current) setShellSyncReport(report);
    } catch (e: any) {
      if (run === shellSyncRun.current) setShellSyncError(String(e));
    } finally {
      if (run === shellSyncRun.current) setShellSyncBusy(false);
    }
  };

  const confirmShellSync = async () => {
    if (!shellSyncReport) return;
    setShellSyncBusy(true);
    setShellSyncError(null);
    try {
      const result = await invoke<ShellSyncPushResult>("shell_sync_push", { sessionId: session.id, pairs: shellSyncPairs });
      const files = `${result.pushed} file${result.pushed === 1 ? "" : "s"} (${fmtBytes(result.bytes)})`;
      const kept = result.backup ? `; the files it replaced are in ${result.backup}` : "";
      if (result.failed.length === 0) {
        addLog(`Shell config: pushed ${files} to ${session.serverName}${kept}.`, "success");
        setShellSyncOpen(false);
      } else {
        addLog(`Shell config: pushed ${files} to ${session.serverName}, ${result.failed.length} failed${kept}.`, "warn");
        setShellSyncResult(result);
      }
    } catch (e: any) {
      setShellSyncError(String(e));
    } finally {
      setShellSyncBusy(false);
    }
  };

  // A push under way: closing then would leave its outcome nowhere to show.
  const shellSyncPushing = shellSyncBusy && !!shellSyncReport;
  const closeShellSync = () => {
    if (shellSyncPushing) return;
    shellSyncRun.current++;
    setShellSyncBusy(false);
    setShellSyncOpen(false);
  };
  // Split-pane state — an ORDERED array of terminal IDs that currently
  // share the main pane. `[]` or a single-id array means "no split, use
  // the usual absolute-overlap layout"; length ≥ 2 means the panes tile
  // side-by-side (or stacked). `activeTab` still tracks which of the split
  // members owns keyboard focus; clicking any pane re-focuses it.
  //
  // We kept this an array instead of a recursive tree — tmux-style
  // arbitrary geometry is neat but 95% of users want "N shells sharing
  // one screen" and a flat layout is trivially resizable, mountable, and
  // reorderable. Add-to-split appends to the end; close-from-split
  // filters that id out.
  const [splitTerminals, setSplitTerminals] = useState<string[]>([]);
  // Horizontal (row) vs vertical (column). The trigger button next to
  // "+" hosts the orientation swap; the same right-click on any divider
  // also flips it.
  const [splitOrientation, setSplitOrientation] = useState<"h" | "v">("h");
  // Per-pane flex ratio, length always equal to `splitTerminals.length`.
  // Sum is arbitrary (we hand it directly to flex-grow so relative
  // proportions are what matters). Add-to-split appends `1` so the new
  // pane comes in equal-sized with its neighbours; drag between two
  // panes redistributes those two panes' share and leaves the rest
  // alone. Reset (double-click a divider) flattens back to all-equal.
  const [splitRatios, setSplitRatios] = useState<number[]>([]);
  // "+" button's secondary menu — the arrow beside "+", or a right-click /
  // long-press on "+", so the primary click stays a fast "new tab" without
  // stealing the advanced flows (split view, compare across servers).
  // Coordinates are window-space, matching the pattern used by the session
  // tab menu at App level. Opened from the keyboard, its first item takes
  // the focus.
  const [plusMenu, setPlusMenu] = useState<{ x: number; y: number; fromKeyboard: boolean } | null>(null);
  const plusGroupRef = useRef<HTMLDivElement>(null);
  const plusArrowRef = useRef<HTMLButtonElement>(null);
  const plusMenuRef = useRef<HTMLDivElement>(null);
  const openPlusMenu = (fromKeyboard: boolean) => {
    const r = plusGroupRef.current?.getBoundingClientRect();
    if (r) setPlusMenu({ x: r.left, y: r.bottom + 4, fromKeyboard });
  };
  // A menu opened from the keyboard gives the focus back to the arrow; one
  // opened with the mouse leaves the focus where it was.
  const closePlusMenu = () => {
    const back = !!plusMenu?.fromKeyboard && !!plusMenuRef.current?.contains(document.activeElement);
    setPlusMenu(null);
    if (back) plusArrowRef.current?.focus({ preventScroll: true });
  };
  // Escape closes it wherever the focus is: after a right-click the focus
  // is still in the terminal, which would send the Escape to the shell.
  useEffect(() => {
    if (!plusMenu) return;
    if (plusMenu.fromKeyboard) plusMenuRef.current?.querySelector<HTMLElement>('[role^="menuitem"]')?.focus({ preventScroll: true });
    const onKey = (e: KeyboardEvent) => {
      if (e.key !== 'Escape') return;
      e.preventDefault();
      e.stopPropagation();
      closePlusMenu();
    };
    window.addEventListener('keydown', onKey, true);
    return () => window.removeEventListener('keydown', onKey, true);
  }, [plusMenu]);
  // A terminal tab's own menu (right-click, or a double-click straight into
  // its name editor). A new title goes into `terminals`, so the Wall and
  // split headers show it too.
  const [termMenu, setTermMenu] = useState<{ x: number; y: number; termId: string } | null>(null);
  const [termRename, setTermRename] = useState<string | null>(null);
  useEffect(() => { if (!termMenu) setTermRename(null); }, [termMenu]);
  const renameTerminal = (termId: string, name: string) => {
    const title = name.trim();
    setTerminals(prev => prev.map(t => {
      if (t.id !== termId) return t;
      if (!title) return t.baseTitle !== undefined ? { ...t, title: t.baseTitle, baseTitle: undefined } : t;
      return { ...t, title, baseTitle: t.baseTitle ?? t.title };
    }));
  };
  // On narrow viewports the side-by-side terminal+tool layout doesn't fit.
  // We collapse to a stacked single-pane view: when a tool is open, the
  // tool takes full width and the terminal is hidden behind a back-chip.
  const isCompact = useIsCompact();
  // Width of the right-side tool pane in pixels. The divider drag updates this
  // and we sync it to localStorage so subsequent sessions remember the split.
  // First-time default is a quarter of the current window width — looks right
  // across both narrow and wide monitors without us picking a magic pixel.
  const [toolPanelWidth, setToolPanelWidth] = useState<number>(() => {
    const saved = parseInt(localStorage.getItem('submarine-tool-panel-width') || '', 10);
    if (Number.isFinite(saved) && saved >= 240) return saved;
    const quarter = Math.round((window.innerWidth || 1440) / 4);
    return Math.max(240, Math.min(900, quarter));
  });
  // Which side of the terminal the tool pane docks on (see DOCK_OPTIONS).
  // Right and left use the width above; above or below, the pane spans the
  // full width and uses its own height.
  const [toolDock, setToolDock] = useState<ToolDock>(() => {
    try {
      const v = localStorage.getItem(TOOL_DOCK_KEY);
      return v === "left" || v === "bottom" || v === "top" ? v : "right";
    } catch { return "right"; }
  });
  const setToolDockPersisted = (d: ToolDock) => {
    setToolDock(d);
    try { localStorage.setItem(TOOL_DOCK_KEY, d); } catch { /* ignore */ }
  };
  const dockTopBottom = toolDock === "bottom" || toolDock === "top";
  // "Full screen": the open tool takes the terminal's place (the button in
  // each tool's title bar). The terminals stay mounted and connected, hidden
  // the way a compact window hides them, and come back when the panel is
  // restored or closed.
  const [toolFull, setToolFull] = useState<boolean>(() => {
    try { return localStorage.getItem(TOOL_FULL_KEY) === "1"; } catch { return false; }
  });
  const setToolFullPersisted = (v: boolean) => {
    setToolFull(v);
    try { localStorage.setItem(TOOL_FULL_KEY, v ? "1" : "0"); } catch { /* ignore */ }
  };
  // The tool is shown without the terminal: always in a compact window, by
  // choice in a roomy one.
  const toolAlone = isCompact || toolFull;
  // Nothing to toggle in a compact window, where the tool is always alone.
  const fullToggle = isCompact ? null : (
    <FullToggle full={toolFull} onToggle={() => setToolFullPersisted(!toolFull)} />
  );
  const [toolPanelHeight, setToolPanelHeight] = useState<number>(() => {
    let saved = NaN;
    try { saved = parseInt(localStorage.getItem('submarine-tool-panel-height') || '', 10); } catch { /* ignore */ }
    if (Number.isFinite(saved) && saved >= MIN_TOOL_HEIGHT) return saved;
    return Math.max(MIN_TOOL_HEIGHT, Math.round((window.innerHeight || 800) * 0.45));
  });
  // Anchor of the open "Panel position" menu, if any.
  const [dockMenu, setDockMenu] = useState<{ x: number; y: number } | null>(null);
  // SFTP pane: the file browser or Mirror (the button in its title bar).
  const [sftpView, setSftpView] = useState<SftpView>("files");
  const mainAreaRef = useRef<HTMLDivElement | null>(null);
  const toolPanelRef = useRef<HTMLDivElement | null>(null);

  const initiatedRef = useRef(false);
  // Guards the per-node "run on connect" commands to fire exactly once for
  // this session view — first successful connect only, never on reconnect.
  const ranOnConnectRef = useRef(false);
  // Whether this tab may type them: null until that first connect decides
  // (asking first when they are held), then true, or false after "Not now".
  const onConnectAllowedRef = useRef<boolean | null>(null);
  // The terminals they went into: each gets them once. With the server's
  // "each new terminal" (#256) that is every terminal of the tab, the ones
  // with their own login too, but not a container's.
  const typedOnConnectRef = useRef<Set<string>>(new Set());
  const terminalsRef = useRef(terminals);
  terminalsRef.current = terminals;
  // Asks before typing on-connect commands someone else set in a shared
  // profile. Read through a ref: the connect listener is registered once.
  const confirm = useConfirm();
  const confirmRef = useRef(confirm);
  confirmRef.current = confirm;
  // Terminals whose shell is open (TerminalView says so): the on-connect
  // commands wait for the first one. Typed before it was open, they were
  // dropped — opening a terminal can take longer than the delay they had.
  const openedTerminalsRef = useRef<Set<string>>(new Set());
  const stopWaitingForTerminalRef = useRef<(() => void) | null>(null);
  // Types the on-connect commands into a terminal whose shell is open, a
  // moment after so it has printed its prompt — they then land after it
  // instead of racing the shell's startup and getting swallowed.
  const typeOnConnectInto = (terminalId: string) => {
    if (typedOnConnectRef.current.has(terminalId)) return;
    typedOnConnectRef.current.add(terminalId);
    const bytes = Array.from(new TextEncoder().encode(onConnectText(session.runOnConnect || '')));
    setTimeout(() => {
      invoke('write_terminal_data', { terminalId, data: bytes }).catch(() => {});
    }, 700);
  };
  // A terminal of this tab that gets them as soon as its shell is open.
  const getsOnConnect = (terminalId: string) =>
    !!session.runOnConnectEvery && onConnectAllowedRef.current === true
    && terminalsRef.current.some(t => t.id === terminalId && !t.container);
  useEffect(() => {
    const onOpened = (e: Event) => {
      const id = (e as CustomEvent).detail;
      if (typeof id !== 'string') return;
      openedTerminalsRef.current.add(id);
      if (getsOnConnect(id)) typeOnConnectInto(id);
    };
    window.addEventListener('submarine-terminal-opened', onOpened);
    return () => {
      window.removeEventListener('submarine-terminal-opened', onOpened);
      stopWaitingForTerminalRef.current?.();
    };
  }, []);

  // ---- Auto-reconnect: fixed 5s cadence, retried FOREVER -------------------
  // After a previously-good session drops we retry every 5 seconds, forever —
  // no backoff ramp, no give-up. The user wants the session/tunnel to keep
  // trying relentlessly on any drop (laptop sleep, mobile-hotspot dropout, a
  // flaky link) so a forward always comes back by itself once the link
  // returns. A failure waiting can't fix (the backend's `retry` is false) or
  // an explicit Cancel stops the cycle: retrying auth-rejected attempts every
  // 5s would just spam the server and risk locking the account, and a host
  // key that wasn't approved would bring its prompt straight back. The
  // terminal/SFTP overlay stays in place the whole time so the user keeps
  // their context.
  //
  // A connect that fails before the session was ever up (a timeout, the
  // server not reachable yet) is retried the same way; the backend's `retry`
  // flag says when waiting can help. Until the first success there's no
  // terminal to keep, so those retries stay in the full-screen connect view.
  const RECONNECT_INTERVAL_MS = 5000;
  const [reconnectAttempt, setReconnectAttempt] = useState(0);
  const [nextReconnectAt, setNextReconnectAt] = useState<number | null>(null);
  const reconnectAttemptRef = useRef(0);
  const reconnectTimerRef = useRef<ReturnType<typeof setTimeout> | null>(null);
  const [everConnected, setEverConnected] = useState(false);
  const everConnectedRef = useRef(false);

  const updateAttempt = (n: number) => {
    reconnectAttemptRef.current = n;
    setReconnectAttempt(n);
  };

  const cancelReconnect = () => {
    if (reconnectTimerRef.current) {
      clearTimeout(reconnectTimerRef.current);
      reconnectTimerRef.current = null;
    }
    updateAttempt(0);
    setNextReconnectAt(null);
  };

  useEffect(() => {
    if (initiatedRef.current) return;
    initiatedRef.current = true;

    primarySeparateRef.current = separateFwdRef.current;

    // Setup listeners. The connection starts once they're all registered (below).
    const unlistenLog = listen(`session-log-${session.id}`, (event: any) => {
      pushLog(event.payload);
    });

    const unlistenPrompt = listen(`fingerprint-prompt-${session.id}`, (event: any) => {
      fpNonceRef.current = event.payload?.nonce ?? null;
      setFingerprintPrompt(event.payload);
    });

    // A dismiss names the prompt it closes (its nonce): one left over from an
    // older connect attempt mustn't close the prompt that's showing now.
    const unlistenPromptDismiss = listen(`fingerprint-prompt-dismiss-${session.id}`, (event: any) => {
      const nonce = event.payload?.nonce;
      if (nonce && fpNonceRef.current && nonce !== fpNonceRef.current) return;
      fpNonceRef.current = null;
      setFingerprintPrompt(null);
    });

    const unlistenKbi = listen(`kbi-prompt-${session.id}`, (event: any) => {
      const p = event.payload;
      kbiNonceRef.current = p?.nonce ?? null;
      setKbiPrompt(p);
      setKbiValues(Array.isArray(p?.prompts) ? p.prompts.map(() => "") : []);
    });

    const unlistenKbiDismiss = listen(`kbi-prompt-dismiss-${session.id}`, (event: any) => {
      const nonce = event.payload?.nonce;
      if (nonce && kbiNonceRef.current && nonce !== kbiNonceRef.current) return;
      kbiNonceRef.current = null;
      setKbiPrompt(null);
      setKbiValues([]);
    });

    // ---- Dedicated secondary connections (separate-sessions mode) ----------
    // These fire only when the setting is on and we opened `::sftp` / `::fwd`.
    // They drive the status dots + auto-retry; routing itself lives in the
    // backend (transport preference), so no keys flip here. The primary
    // session's own KBI / fingerprint prompts stay wired above.
    const unlistenSftpOk = listen(`connection-success-${session.id}::sftp`, () => {
      sftpRetryRef.current = 0;
      setSftpConnStatus('ready');
    });
    const unlistenSftpFail = listen(`connection-failed-${session.id}::sftp`, (event: any) => {
      // Auth-style failures (2FA-only server, bad credentials) won't get
      // better on retry — go straight to failed; SFTP rides the primary.
      if (event.payload?.is_auth_error) setSftpConnStatus('failed');
      else if (separateSftpRef.current && statusRef.current === 'connected') scheduleSecondaryRetry('sftp');
      else setSftpConnStatus('off');
    });
    const unlistenSftpDown = listen(`session-disconnected-${session.id}::sftp`, (event: any) => {
      // Unexpected transport death (the backend already dropped the base SFTP
      // cache, so file ops already fall back to the primary) — try to bring
      // the dedicated connection back. A user-initiated disconnect (toggle
      // off) must never retry.
      const userInitiated = !!event.payload?.user_initiated;
      if (!userInitiated && separateSftpRef.current && statusRef.current === 'connected') scheduleSecondaryRetry('sftp');
      else setSftpConnStatus('off');
    });

    const unlistenFwdOk = listen(`connection-success-${session.id}::fwd`, () => {
      fwdRetryRef.current = 0;
      setFwdConnStatus('ready');
      // Tunnel migration onto the fresh transport happens backend-side.
    });
    const unlistenFwdFail = listen(`connection-failed-${session.id}::fwd`, (event: any) => {
      if (event.payload?.is_auth_error) {
        // Won't get better on retry — run the tunnels on the primary instead.
        setFwdConnStatus('failed');
        invoke("restart_session_tunnels", { sessionId: session.id, serverId: session.serverId }).catch(console.error);
      } else if (separateFwdRef.current && statusRef.current === 'connected') {
        // scheduleSecondaryRetry falls back to the primary (and restores the
        // tunnels there) once the retry budget is exhausted.
        scheduleSecondaryRetry('fwd');
      } else {
        setFwdConnStatus('off');
      }
    });
    const unlistenFwdDown = listen(`session-disconnected-${session.id}::fwd`, (event: any) => {
      // The watcher already stopped the tunnels that rode this transport.
      // Retry brings the transport back (backend migration restarts them on
      // it); exhausted retries restore them on the primary. A user-initiated
      // disconnect (toggle off — which restores tunnels itself) never retries.
      const userInitiated = !!event.payload?.user_initiated;
      if (!userInitiated && separateFwdRef.current && statusRef.current === 'connected') scheduleSecondaryRetry('fwd');
      else setFwdConnStatus('off');
    });

    const unlistenSuccess = listen(`connection-success-${session.id}`, () => {
      // A reconnect is a success after an earlier one. Retries of a first
      // connect that never got through are still the first connect: the
      // terminals open fresh and the on-connect commands run.
      const wasReconnect = everConnectedRef.current;
      everConnectedRef.current = true;
      setEverConnected(true);
      setStatus('connected');
      cancelReconnect();
      // Whatever got this connection in — the password typed into the failed
      // screen, or one asked for — is in the tab's cache now, so forget the
      // typed override before the secondaries below start: a mistyped one
      // would otherwise be tried first on every reconnect and secondary
      // connection, a failed login each time.
      customPasswordRef.current = "";
      setCustomPassword("");
      // On a successful RECONNECT we bump connectionEpoch instead of
      // replacing the terminals array. The terminal_id stays the same
      // (so the existing event listener keeps catching output), the
      // xterm instance is preserved (so the user's scroll-back survives
      // the drop), and TerminalView's per-epoch effect re-invokes
      // open_terminal — backend's terminal_txs HashMap.insert replaces
      // the dead entry transparently so the user's next keystroke
      // routes into the fresh PTY.
      if (wasReconnect) {
        setConnectionEpoch(e => e + 1);
      } else if (session.runOnConnect && session.runOnConnect.trim() && !ranOnConnectRef.current) {
        // Per-node on-connect commands: auto-typed ONCE into the first
        // terminal on the INITIAL connect only (the `else` gates out
        // reconnects; ranOnConnectRef gates out any duplicate success
        // event), once the terminal's shell is open (typeOnConnectInto).
        // With "each new terminal", every terminal of the tab gets them
        // once its shell is open (getsOnConnect), also the ones open already.
        ranOnConnectRef.current = true;
        const cmds = onConnectText(session.runOnConnect);
        const typeThem = () => {
          onConnectAllowedRef.current = true;
          if (session.runOnConnectEvery) {
            for (const t of terminalsRef.current) {
              if (!t.container && openedTerminalsRef.current.has(t.id)) typeOnConnectInto(t.id);
            }
          }
          const terminalId = `${session.id}-term-0`;
          if (openedTerminalsRef.current.has(terminalId)) {
            typeOnConnectInto(terminalId);
            return;
          }
          const onOpened = (e: Event) => {
            if ((e as CustomEvent).detail !== terminalId) return;
            stopWaitingForTerminalRef.current?.();
            typeOnConnectInto(terminalId);
          };
          window.addEventListener('submarine-terminal-opened', onOpened);
          stopWaitingForTerminalRef.current = () => {
            window.removeEventListener('submarine-terminal-opened', onOpened);
            stopWaitingForTerminalRef.current = null;
          };
        };
        if (session.runOnConnectHeld) {
          // These commands were set or changed elsewhere (a co-member, an
          // imported file) and this device hasn't allowed them: ask, never type
          // them unasked. Cancel has the focus, so a stray Enter is a no.
          confirmRef.current({
            title: "Run the on-connect commands?",
            message: `These commands were set elsewhere to run when you connect to ${session.serverName}, and this device hasn't allowed them:\n\n${cmds.trimEnd()}\n\nThey would be typed into your terminal on that server${session.runOnConnectEvery ? ", and into each new terminal you open there" : ""}.`,
            okLabel: "Allow and run",
            cancelLabel: "Not now",
            safeDefault: true,
          }).then(async (ok) => {
            if (!ok) {
              onConnectAllowedRef.current = false;
              addLog(`On-connect commands for ${session.serverName} not run: they are waiting for your OK.`, "info");
              return;
            }
            try {
              await invoke("allow_held", { serverId: session.serverId, items: ["run_on_connect"] });
              window.dispatchEvent(new Event("submarine-held-changed"));
            } catch (e) {
              onConnectAllowedRef.current = false;
              addLog(`ALLOW_FAILED: ${e}`, "error");
              return;
            }
            typeThem();
          });
        } else {
          typeThem();
        }
      }
      // The handshake may have inserted a row into `known_hosts` (user just
      // accepted a new fingerprint). Flush the encrypted vault so the entry
      // survives an app restart — otherwise the prompt would reappear every
      // session, which was especially painful for SOCKS-proxied connections.
      invoke("persist_vault").catch(console.error);
      // Separate-sessions: now that the primary is up (and the host key is
      // saved, so no double fingerprint prompt), spin up the dedicated SFTP /
      // forwarding connections. No-op when the setting is off. Fires on
      // reconnect too — the backend already reaped the stale secondaries.
      resetSecondaryStatus();
      openSecondaryConnections();
      // Reconcile the toggled-off-during-handshake case: the primary deferred
      // its saved tunnels for a `::fwd` connection we are NOT going to open, so
      // nobody would start them — restore on the primary.
      if (primarySeparateRef.current && !separateFwdRef.current) {
        restoreTunnelsOnPrimary();
      }
    });

    const unlistenFailed = listen(`connection-failed-${session.id}`, (event: any) => {
      pushLog({ msg: `Connection failed: ${event.payload?.reason}`, type: 'error' });
      const isAuth = !!event.payload?.is_auth_error;
      setStatus('failed');
      setIsAuthError(isAuth);
      resetSecondaryStatus();
      addLog(`SSH_CONNECTION_FAILED [${session.serverName}]: ${event.payload?.reason}`, "error");
      // Try again in a few seconds — the first connect too — when waiting can
      // help: a timeout, a refusal, a link that broke. A rejected login, a
      // host key that wasn't approved (its prompt would come straight back)
      // or a setting stops the cycle; Reconnect is still there.
      if (event.payload?.retry === true) {
        scheduleReconnect(reconnectAttemptRef.current + 1);
      } else {
        cancelReconnect();
      }
    });

    // Backend fires `session-disconnected-{id}` from the keepalive watcher when
    // a previously-good session is detected as closed (network drop, server
    // kill, idle timeout). Flip into a frozen state — terminals + SFTP are
    // disabled until the user reconnects or closes the tab. Kick off
    // auto-reconnect immediately; the banner shows the countdown.
    const unlistenDisconnected = listen(`session-disconnected-${session.id}`, (event: any) => {
      const reason = event.payload?.reason || "Connection lost";
      const userInitiated = !!event.payload?.user_initiated;
      setStatus('disconnected');
      setDisconnectReason(reason);
      resetSecondaryStatus();
      addLog(`SSH_DISCONNECTED [${session.serverName}]: ${reason}`, "error");
      // User-initiated disconnect (right-click → Disconnect) should NOT
      // bounce straight into auto-reconnect — the user explicitly asked
      // for the session to be dead. The "Reconnect" banner button is
      // still available if they change their mind.
      if (!userInitiated) {
        scheduleReconnect(1);
      } else {
        cancelReconnect();
      }
    });

    // Start connection — only once every listener above is registered. A
    // connection can fail before any network round trip (a ~/.ssh/config
    // value that's refused, say) and report it at once; a report that comes
    // before its listener is lost, and the tab would sit on "Connecting…" for
    // good. We read from the ref so a password the user types into the
    // auth-retry input is picked up by later attempts inside this same effect
    // closure (the effect itself only runs once).
    Promise.all([
      unlistenLog, unlistenPrompt, unlistenPromptDismiss, unlistenKbi, unlistenKbiDismiss,
      unlistenSftpOk, unlistenSftpFail, unlistenSftpDown, unlistenFwdOk, unlistenFwdFail, unlistenFwdDown,
      unlistenSuccess, unlistenFailed, unlistenDisconnected,
    ]).then(() =>
      invoke("initiate_connection", { sessionId: session.id, serverId: session.serverId, customPassword: customPasswordRef.current || null, quickAuth: session.quickAuth || null, sessionRole: "primary", separateSessions: separateFwdRef.current, useSshAgent: readUseSshAgent() })
    ).catch(e => {
      pushLog({ msg: `Failed to initiate: ${e}`, type: 'error' });
      setStatus('failed');
    });

    return () => {
      unlistenLog.then(f => f());
      unlistenPrompt.then(f => f());
      unlistenPromptDismiss.then(f => f());
      unlistenKbi.then(f => f());
      unlistenKbiDismiss.then(f => f());
      unlistenSftpOk.then(f => f());
      unlistenSftpFail.then(f => f());
      unlistenSftpDown.then(f => f());
      unlistenFwdOk.then(f => f());
      unlistenFwdFail.then(f => f());
      unlistenFwdDown.then(f => f());
      unlistenSuccess.then(f => f());
      unlistenFailed.then(f => f());
      unlistenDisconnected.then(f => f());

      if (reconnectTimerRef.current) clearTimeout(reconnectTimerRef.current);
      clearSecondaryRetryTimers();
      invoke("disconnect_session", { sessionId: session.id }).catch(console.error);
    };
  }, [session.id, session.serverId]);

  const scheduleReconnect = (attempt: number) => {
    if (reconnectTimerRef.current) {
      clearTimeout(reconnectTimerRef.current);
      reconnectTimerRef.current = null;
    }
    const delay = RECONNECT_INTERVAL_MS;
    updateAttempt(attempt);
    setNextReconnectAt(Date.now() + delay);
    reconnectTimerRef.current = setTimeout(() => {
      setNextReconnectAt(null);
      setStatus('connecting');
      // Seen in the connect view while a first connect is being retried.
      pushLog({ msg: `Trying again (attempt ${attempt})…`, type: 'info' });
      primarySeparateRef.current = separateFwdRef.current;
      invoke("initiate_connection", {
        sessionId: session.id,
        serverId: session.serverId,
        customPassword: customPasswordRef.current || null,
        quickAuth: session.quickAuth || null,
        sessionRole: "primary",
        separateSessions: separateFwdRef.current,
        useSshAgent: readUseSshAgent(),
      }).catch((e) => {
        // Refused before any connecting (the server was deleted, say): no
        // failure event follows, so don't sit on "Connecting…".
        pushLog({ msg: `Failed to initiate: ${e}`, type: 'error' });
        setStatus('failed');
        cancelReconnect();
      });
    }, delay);
  };

  const reconnect = () => {
    // Once the session has been up, a reconnect (the banner's Now, or
    // Reconnect) is one more attempt of the cycle: the terminals stay on
    // screen behind the banner — the full-screen connect view would unmount
    // them and lose their scrollback — and a timeout goes on retrying.
    const attempt = everConnectedRef.current ? Math.max(1, reconnectAttemptRef.current) : 0;
    cancelReconnect();
    updateAttempt(attempt);
    setStatus('connecting');
    setLogs([]);
    setIsAuthError(false);
    // A prompt still showing belongs to the attempt being replaced; the
    // backend stops it from prompting or reporting once this one starts.
    fpNonceRef.current = null;
    kbiNonceRef.current = null;
    setFingerprintPrompt(null);
    setKbiPrompt(null);
    setKbiValues([]);
    setDisconnectReason("");
    resetSecondaryStatus();
    primarySeparateRef.current = separateFwdRef.current;
    invoke("initiate_connection", {
      sessionId: session.id,
      serverId: session.serverId,
      customPassword: customPassword || null,
      quickAuth: session.quickAuth || null,
      sessionRole: "primary",
      separateSessions: separateFwdRef.current,
      useSshAgent: readUseSshAgent(),
    }).catch(console.error);
  };

  // Tab-strip right-click "Reconnect" fires a DOM CustomEvent rather than
  // round-tripping through React props — keeps DesktopApp's tab UI decoupled
  // from SessionView's reconnect lifecycle.
  useEffect(() => {
    const onReconnect = () => reconnect();
    const evt = `session-reconnect-${session.id}`;
    window.addEventListener(evt, onReconnect);
    return () => window.removeEventListener(evt, onReconnect);
  }, [session.id, customPassword]);

  // ---- Tool pane sizing ----------------------------------------------------
  // The pane and the terminal share the window: opening the pane or dragging
  // its divider sizes the two against each other. The window itself is never
  // resized from here. Its size belongs to the user, and the window-state
  // plugin restores it.

  const startToolResize = (e: React.MouseEvent) => {
    e.preventDefault();
    if (dockTopBottom) {
      // Dragging toward the terminal grows the pane.
      const startY = e.clientY;
      const area = mainAreaRef.current?.clientHeight ?? window.innerHeight;
      const maxHeight = Math.max(MIN_TOOL_HEIGHT, area - MIN_TERMINAL_HEIGHT - 4);
      // Start from what's on screen: a height saved in a taller window
      // shows capped.
      const startHeight = Math.min(maxHeight, toolPanelRef.current?.getBoundingClientRect().height ?? toolPanelHeight);
      const sign = toolDock === "bottom" ? 1 : -1;
      let latest = startHeight;
      const onMove = (ev: MouseEvent) => {
        latest = Math.round(Math.max(MIN_TOOL_HEIGHT, Math.min(maxHeight, startHeight + sign * (startY - ev.clientY))));
        setToolPanelHeight(latest);
      };
      const onUp = () => {
        window.removeEventListener("mousemove", onMove);
        window.removeEventListener("mouseup", onUp);
        try { localStorage.setItem("submarine-tool-panel-height", String(latest)); }
        catch { /* ignore */ }
      };
      window.addEventListener("mousemove", onMove);
      window.addEventListener("mouseup", onUp);
      return;
    }
    const startX = e.clientX;
    const area = mainAreaRef.current?.clientWidth ?? window.innerWidth;
    const maxWidth = Math.max(240, Math.min(900, area - MIN_TERMINAL_WIDTH - 4));
    // Start from what's on screen: a width saved in a wider window shows
    // capped.
    const startWidth = Math.min(maxWidth, toolPanelRef.current?.getBoundingClientRect().width ?? toolPanelWidth);
    let latest = startWidth;

    const onMove = (ev: MouseEvent) => {
      // Dragging toward the terminal widens the tool pane: left for a pane
      // on the right, right for a pane on the left.
      const pulled = toolDock === "left" ? ev.clientX - startX : startX - ev.clientX;
      latest = Math.round(Math.max(240, Math.min(maxWidth, startWidth + pulled)));
      setToolPanelWidth(latest);
    };

    const onUp = () => {
      window.removeEventListener("mousemove", onMove);
      window.removeEventListener("mouseup", onUp);
      try { localStorage.setItem("submarine-tool-panel-width", String(latest)); }
      catch { /* ignore */ }
    };

    window.addEventListener("mousemove", onMove);
    window.addEventListener("mouseup", onUp);
  };

  const handleFingerprintResponse = async (accepted: boolean) => {
    // The `nonce` came from the prompt event and binds this response 1:1
    // to the connect attempt that emitted it. The backend ignores any
    // response whose nonce isn't in its in-flight map, so stale clicks
    // (or a hostile script that knows only the session id) can't accept
    // a fingerprint on the user's behalf.
    const nonce = fingerprintPrompt?.nonce;
    fpNonceRef.current = null;
    setFingerprintPrompt(null);
    if (!nonce) return;
    try {
      await invoke("verify_fingerprint_response", { nonce, accepted });
    } catch (e) {
      console.error(e);
    }
  };

  // Keyboard-interactive (2FA) submit / cancel. Like the fingerprint flow the
  // nonce binds the answer to this exact connect attempt; cancel sends `null`
  // so the backend aborts the interactive auth instead of waiting out its
  // 120s timeout.
  const handleKbiSubmit = async () => {
    const nonce = kbiPrompt?.nonce;
    const responses = kbiValues;
    kbiNonceRef.current = null;
    setKbiPrompt(null);
    setKbiValues([]);
    if (!nonce) return;
    try {
      await invoke("submit_kbi_response", { nonce, responses });
    } catch (e) {
      console.error(e);
    }
  };

  const handleKbiCancel = async () => {
    const nonce = kbiPrompt?.nonce;
    kbiNonceRef.current = null;
    setKbiPrompt(null);
    setKbiValues([]);
    if (!nonce) return;
    try {
      await invoke("submit_kbi_response", { nonce, responses: null });
    } catch (e) {
      console.error(e);
    }
  };

  // Host-key fingerprint + keyboard-interactive (2FA) prompts, held in ONE
  // place so they render both in the first-connect full-screen view AND — via
  // a portal overlay (see the connected-state return) — during an auto-
  // reconnect. Previously these only lived inside the reconnectAttempt===0
  // early-return, so a changed-host-key or 2FA prompt fired mid-reconnect was
  // never drawn (the slim banner has no prompt UI), leaving the user unable to
  // respond and the reconnect cycle spinning.
  const authPrompts = (
    <AuthPrompts
      fingerprintPrompt={fingerprintPrompt}
      kbiPrompt={kbiPrompt}
      kbiValues={kbiValues}
      setKbiValues={setKbiValues}
      kbiFirstInputRef={kbiFirstInputRef}
      handleFingerprintResponse={handleFingerprintResponse}
      handleKbiSubmit={handleKbiSubmit}
      handleKbiCancel={handleKbiCancel}
      serverName={session.serverName}
      displayName={displayName}
    />
  );

  // Host-key / 2FA / login prompts float over THIS session's area — centered
  // and scrolling on their own, whatever the log's scroll — but never over the
  // title bar, another tab or another split pane, and below the app's own
  // dialogs. A background tab's prompt stays hidden with its tab (the tab
  // strip marks it via onPromptChange), so it can't pop up over whatever the
  // user is doing. No aria-modal: it only blocks this session.
  const promptOverlay = (
    <div
      role="dialog"
      aria-label={`Connection prompt for ${session.serverName}`}
      className="absolute inset-0 z-40 flex items-start justify-center bg-black/70 backdrop-blur-sm p-4 sm:p-8 overflow-y-auto"
    >
      <div className="max-w-2xl w-full mt-6 sm:mt-12">
        {authPrompts}
      </div>
    </div>
  );

  // Only render the full-screen log view for the FIRST connection (retries of
  // it included) — once an auto-reconnect cycle is running after the session
  // was up, the user's terminal output and SFTP state stay visible behind a
  // slim banner.
  if ((reconnectAttempt === 0 || !everConnected) && (status === 'connecting' || status === 'failed')) {
    const retryWaiting = reconnectAttempt > 0 && nextReconnectAt !== null;
    const retryIn = <SecondsLeft until={nextReconnectAt ?? 0} />;
    return (
      <div className="relative flex-1 flex flex-col p-4 sm:p-8 bg-[#0a0a0c] text-white overflow-hidden">
        <div className="max-w-2xl w-full mx-auto flex-1 flex flex-col min-h-0">
          {/* Header: status chip + server name on the left, actions on the
              right — one row at every width. The name is regular-case text
              that truncates instead of wrapping (a long user@host used to
              wrap and push the buttons onto their own line), and on phones
              the buttons collapse to icons. Close is there while connecting
              too, so a hung attempt can be abandoned from this screen. */}
          <div className="flex items-center gap-3 mb-3 sm:mb-4">
            <div
              className={`shrink-0 w-9 h-9 rounded-xl flex items-center justify-center ${
                status === 'failed' ? 'bg-red-500/10 text-red-400' : 'bg-primary/10 text-primary'
              }`}
              aria-hidden="true"
            >
              {status === 'connecting' ? <Loader2 size={16} className="animate-spin-brief" /> : <AlertTriangle size={16} />}
            </div>
            <div className="min-w-0 flex-1">
              <h2 className="text-sm font-semibold text-zinc-100 truncate" title={session.serverName}>
                {displayName || session.serverName}
              </h2>
              {/* One line on a phone too: the retry states have a short form
                  there, since the buttons beside it leave little room. */}
              <p className={`text-[11px] mt-0.5 truncate ${status === 'failed' ? 'text-red-400/90' : 'text-zinc-500'}`}>
                {status === 'connecting'
                  ? (reconnectAttempt > 0
                    ? <><span className="sm:hidden">Trying again · #{reconnectAttempt}</span><span className="hidden sm:inline">Trying again · attempt {reconnectAttempt}</span></>
                    : 'Connecting…')
                  : retryWaiting
                    ? <><span className="sm:hidden">Failed · retry in {retryIn}s</span><span className="hidden sm:inline">Connection failed · trying again in {retryIn}s</span></>
                    : 'Connection failed'}
              </p>
            </div>
            <div className="shrink-0 flex items-center gap-2">
              {retryWaiting && (
                <button
                  onClick={cancelReconnect}
                  title="Stop trying again"
                  aria-label="Stop trying again"
                  className="h-8 px-2.5 sm:px-3 flex items-center gap-1.5 rounded-lg bg-white/5 text-zinc-300 hover:bg-white/10 hover:text-white text-xs font-semibold transition-colors"
                >
                  <CircleSlash size={13} /><span className="hidden sm:inline">Cancel</span>
                </button>
              )}
              {status === 'failed' && (
                <button
                  onClick={reconnect}
                  title="Reconnect"
                  aria-label="Reconnect"
                  className="h-8 px-2.5 sm:px-3 flex items-center gap-1.5 rounded-lg bg-primary/10 text-primary hover:bg-primary/20 text-xs font-semibold transition-colors"
                >
                  <RotateCw size={13} /><span className="hidden sm:inline">Reconnect</span>
                </button>
              )}
              <button
                onClick={onClose}
                title="Close session"
                aria-label="Close session"
                className="h-8 px-2.5 sm:px-3 flex items-center gap-1.5 rounded-lg bg-white/5 text-zinc-300 hover:bg-red-500/15 hover:text-red-300 text-xs font-semibold transition-colors"
              >
                <X size={13} /><span className="hidden sm:inline">Close</span>
              </button>
            </div>
          </div>

          {/* Auth failed: try another password without leaving the screen. Its
              own row, so it never squeezes the header. Enter reconnects. */}
          {status === 'failed' && isAuthError && (
            <input
              type="password"
              aria-label="Password"
              placeholder="Try another password — press Enter to reconnect"
              className="h-8 w-full mb-3 sm:mb-4 bg-[#1a1a1e] rounded-lg px-3 text-xs text-white border border-white/10 outline-none focus:border-primary/50"
              value={customPassword}
              onChange={e => setCustomPassword(e.target.value)}
              onKeyDown={e => {
                if (e.key === 'Enter') reconnect();
              }}
            />
          )}

          {/* Log Window. `min-h-0` is what makes it actually scroll: a flex
              child defaults to min-height:auto (its content height), so without
              this the box grows to fit every line and the parent's
              overflow-hidden clips it instead of the inner overflow-y-auto
              taking over — invisible on desktop with a tall window, but on a
              short Android viewport the log gets cut off with no way to scroll.
              overscroll-contain keeps a flick from scrolling the page behind it;
              WebkitOverflowScrolling gives older Android WebViews momentum. */}
          <div
            ref={attachLogBox}
            onScroll={(e) => {
              // Follow only while (nearly) at the bottom — a few px of slack
              // for sub-pixel rounding after our own scroll.
              const el = e.currentTarget;
              logFollowRef.current = el.scrollHeight - el.scrollTop - el.clientHeight < 32;
            }}
            className="flex-1 min-h-0 bg-[#121214] border border-white/5 rounded-2xl p-4 font-mono overflow-y-auto overscroll-contain custom-scrollbar shadow-inner relative select-text cursor-text"
            style={{ WebkitOverflowScrolling: 'touch', fontSize: logFontSize, fontFamily: logFontFamily, lineHeight: 1.5 }}
          >
            {logs.map((l, i) => (
              <div key={i} className={`mb-2 ${l.type === 'error' ? 'text-red-400' : l.type === 'success' ? 'text-primary' : 'text-zinc-400'}`}>
                <span className="text-zinc-600 opacity-50 mr-3">[{l.time}]</span>
                {l.msg}
              </div>
            ))}
          </div>

          {/* The prompt floats over this session instead of being appended to
              the bottom of the scroll-box above, where a short window — or a
              long keyboard-interactive banner before the code prompt — pushed
              the input below the fold and the connection looked "stuck at
              2FA" (#27). See promptOverlay. */}
          {promptWaiting && promptOverlay}
        </div>
      </div>
    );
  }

  const DockIcon = (DOCK_OPTIONS.find((o) => o.id === toolDock) ?? DOCK_OPTIONS[0]).Icon;

  // Connected State with Nested Tabs
  return (
    <div className="relative flex-1 flex flex-col bg-background overflow-hidden animate-in fade-in">
      {/* Auth prompts during an AUTO-RECONNECT. The full-screen view above only
          renders on the first connect (reconnectAttempt===0); once a reconnect
          cycle is running we show the terminal behind a slim banner, so the
          prompt floats over it — see promptOverlay. */}
      {promptWaiting && promptOverlay}
      {/* Nested Tab Bar — hidden in `chromeless` mode. Chromeless is
          used by the App-level Split-view tiling: merged (non-focused)
          panes show only the active terminal, no per-session tab strip
          + tool rail. Otherwise every merged tile would carry its own
          full chrome and 2-3 sessions side-by-side would fight for
          vertical space with duplicate toolbars. */}
      {!chromeless && (
      <div inert={shellSyncOpen ? "" : undefined} className="h-12 border-b border-white/5 bg-[#121214]/50 flex items-center px-2 sm:px-4 shrink-0 justify-between gap-1">
        <div
          className="flex items-center gap-1 overflow-x-auto no-scrollbar flex-1 mr-1 sm:mr-4 mask-fade-right"
          onWheel={(e) => { e.currentTarget.scrollLeft += e.deltaY; }}
        >
        {terminals.map(t => {
          // Highlight every terminal currently participating in the split
          // — the keyboard-focused one gets the primary tint, the others
          // a softer accent, so the pairing is obvious at a glance.
          const isFocusedHalf = activeTab === t.id;
          const isSplitPartner = !!(splitTerminals.includes(t.id) && !isFocusedHalf);
          return (
          <div key={t.id} className="group relative flex items-center"
            onContextMenu={(e) => { e.preventDefault(); setTermMenu({ x: e.clientX, y: e.clientY, termId: t.id }); }}
            onDoubleClick={(e) => { setTermMenu({ x: e.clientX, y: e.clientY, termId: t.id }); setTermRename(t.id); }}
          >
            <button
              onClick={() => {
                // Clicking a tab NOT participating in the current split
                // exits split mode entirely — split is scoped to the
                // panes it was built with; jumping elsewhere means the
                // user wants a plain full-pane view.
                if (splitTerminals.length >= 2 && !splitTerminals.includes(t.id)) {
                  setSplitTerminals([]);
                  setSplitRatios([]);
                }
                setActiveTab(t.id);
                // Land the caret in the terminal even when the active one
                // didn't change (re-clicking the current tab, or focus was in
                // a tool pane / another input) — issue #51.
                requestTerminalFocus();
              }}
              title={t.container ? `Container: ${t.container.name}` : undefined}
              className={`h-8 px-3 sm:px-4 ${terminals.length > 1 ? 'pr-8' : ''} rounded-lg flex items-center gap-2 text-[11px] font-bold uppercase tracking-wider transition-all ${
                isFocusedHalf
                  ? 'bg-primary/10 text-primary border border-primary/20 shadow-inner'
                  : isSplitPartner
                    ? 'bg-primary/[0.04] text-primary/70 border border-primary/10'
                    : 'text-zinc-300 bg-white/[0.04] border border-white/10 hover:bg-white/[0.08] hover:border-white/20 hover:text-white'
              } ${t.container ? 'ring-1 ring-sky-400/30' : ''}`}
            >
              {t.container
                ? <Container size={14} className="text-sky-300" />
                : <TerminalSquare size={14} />}
              {t.title}
              {isSplitPartner && (
                <SplitSquareHorizontal size={10} className="text-primary/60" />
              )}
            </button>
            {terminals.length > 1 && (
              <button
                onClick={(e) => {
                  e.stopPropagation();
                  setTerminals(prev => prev.filter(x => x.id !== t.id));
                  if (activeTab === t.id) setActiveTab(terminals[0].id);
                  // Closing a terminal that's participating in the split
                  // has to drop it from splitTerminals AND splitRatios or
                  // the flex layout renders a torn-down partner.
                  if (splitTerminals.includes(t.id)) {
                    const idx = splitTerminals.indexOf(t.id);
                    const nextTerms = splitTerminals.filter(id => id !== t.id);
                    const nextRatios = splitRatios.filter((_, i) => i !== idx);
                    // Falling below 2 members collapses back to the plain
                    // absolute-overlap layout.
                    if (nextTerms.length < 2) {
                      setSplitTerminals([]);
                      setSplitRatios([]);
                    } else {
                      setSplitTerminals(nextTerms);
                      setSplitRatios(nextRatios);
                    }
                  }
                }}
                aria-label="Close terminal"
                className="absolute right-1 top-1/2 -translate-y-1/2 w-5 h-5 flex items-center justify-center rounded opacity-100 sm:opacity-0 sm:group-hover:opacity-100 sm:focus-visible:opacity-100 hover:bg-white/5 hover:text-red-400 text-zinc-500 transition-all"
              >
                <X size={12} />
              </button>
            )}
          </div>
          );
        })}

          {/* "+" for all terminal-creation flavours. A click adds a plain
              tab (the 90% case). The arrow beside it — or a right-click /
              long-press on "+" — opens the menu with the advanced modes:
              split-with-new-pane, un-split, and a terminal with its own
              login (once, or always for this server). A right-click alone
              went unnoticed (#243); the arrow also lets the keyboard in. */}
          <div ref={plusGroupRef} className="ml-1 shrink-0 flex items-stretch rounded-lg border border-dashed border-white/10">
            <button
              onClick={() => {
                const newId = `${session.id}-term-${Date.now()}`;
                setTerminals(prev => [...prev, { id: newId, title: nextTerminalNumber(prev), ownLogin: ownLoginDefault }]);
                setActiveTab(newId);
              }}
              onContextMenu={(e) => {
                e.preventDefault();
                openPlusMenu(false);
              }}
              className="h-10 w-10 sm:h-8 sm:w-8 rounded-l-lg flex items-center justify-center text-zinc-500 hover:bg-white/10 hover:text-white transition-all relative"
              title="New terminal"
              aria-label="New terminal"
            >
              <Plus size={14} />
              {splitTerminals.length >= 2 && (
                <span
                  className="absolute -top-1 -right-1 min-w-[14px] h-[14px] px-1 rounded-full bg-primary text-[9px] font-bold text-black flex items-center justify-center leading-none"
                  aria-hidden="true"
                  title={`Split active (${splitTerminals.length} panes)`}
                >
                  {splitTerminals.length}
                </span>
              )}
            </button>
            <button
              ref={plusArrowRef}
              // A click from the keyboard (Enter, Space) has no mouse detail.
              onClick={(e) => openPlusMenu(e.detail === 0)}
              aria-haspopup="menu"
              aria-expanded={!!plusMenu}
              aria-label="More terminal options"
              title="More terminal options"
              className="h-10 w-7 sm:h-8 sm:w-5 rounded-r-lg border-l border-dashed border-white/10 flex items-center justify-center text-zinc-500 hover:bg-white/10 hover:text-white transition-all"
            >
              <ChevronDown size={12} />
            </button>
          </div>
        </div>

        {/* Tool rail: icon + text label. The label collapses to icon-only on
            narrow viewports (below md) so the terminal tab strip keeps its
            horizontal real estate when space is tight; the full description
            stays in the tooltip either way. */}
        <div className="flex items-center gap-1 shrink-0 sm:border-l border-white/5 sm:pl-3 pl-1">
          {([
            { id: 'sftp',    icon: Folder,  label: 'SFTP',    hint: 'SFTP — file browser' },
            { id: 'tunnels', icon: Network, label: 'Ports',   hint: 'Ports — port forwarding' },
            { id: 'cmds',    icon: Library, label: 'Library', hint: 'Library — commands & notes' },
            { id: 'info',    icon: Info,    label: 'Info',    hint: 'Info — server overview' },
          ] as const).map(({ id, icon: Icon, label, hint }) => {
            const on = activeTool === id;
            return (
              <button
                key={id}
                onClick={() => setActiveTool(on ? null : id)}
                title={hint}
                aria-label={hint}
                aria-pressed={on}
                className={`h-10 sm:h-8 px-2.5 rounded-lg flex items-center gap-1.5 transition-all ${
                  on
                    ? 'bg-primary/10 text-primary border border-primary/20 shadow-inner'
                    : 'text-zinc-300 bg-white/[0.04] border border-white/10 hover:bg-white/[0.08] hover:border-white/20 hover:text-white'
                }`}
              >
                <Icon size={14} className="shrink-0" />
                <span className="hidden md:inline text-[11px] font-semibold tracking-tight">{label}</span>
              </button>
            );
          })}
          {/* "Push shell config": only on the servers picked in Settings →
              Shell config sync, not all of them. Unlike the four tabs above
              it doesn't dock a panel: it's a one-shot push, so it opens a
              preview/confirm dialog instead (see shellSyncOpen below). */}
          {shellSyncOffered && (
            <button
              onClick={openShellSync}
              disabled={status !== 'connected'}
              title="Push shell config: your files from Settings → Shell config sync, to this server"
              aria-label="Push shell config"
              className="h-10 sm:h-8 px-2.5 rounded-lg flex items-center gap-1.5 transition-all text-zinc-300 bg-white/[0.04] border border-white/10 hover:bg-white/[0.08] hover:border-white/20 hover:text-white disabled:opacity-40 disabled:pointer-events-none"
            >
              <UploadCloud size={14} className="shrink-0" />
              <span className="hidden md:inline text-[11px] font-semibold tracking-tight">Shell</span>
            </button>
          )}
          {/* Where the tool pane sits around the terminal. Compact windows
              show the tool full screen, so there is nothing to place. */}
          {!isCompact && (
            <button
              onClick={(e) => {
                const r = e.currentTarget.getBoundingClientRect();
                setDockMenu({ x: r.right, y: r.bottom + 4 });
              }}
              title="Panel position"
              aria-label="Panel position"
              aria-haspopup="menu"
              className="h-10 sm:h-8 px-2 rounded-lg flex items-center text-zinc-400 border border-transparent hover:bg-white/[0.08] hover:border-white/20 hover:text-white transition-all"
            >
              <DockIcon size={14} />
            </button>
          )}
        </div>
      </div>
      )}

      {/* "+" secondary menu — the arrow beside "+", or a right-click /
          long-press on it. Kept intentionally short: same-server "split
          with new pane", the h/v orientation flip, and un-split. Cross-server
          split lives on the session tab strip (right-click any tab
          → the picker); one entry point per concept keeps the menu
          scannable. Everything else is one primary click on "+". */}
      {plusMenu && createPortal(
        <>
          <div
            className="fixed inset-0 z-[9998]"
            onClick={() => closePlusMenu()}
            onContextMenu={(e) => { e.preventDefault(); closePlusMenu(); }}
          />
          <div
            ref={plusMenuRef}
            role="menu"
            aria-label="Terminal options"
            onKeyDown={(e) => {
              const items = Array.from(plusMenuRef.current?.querySelectorAll<HTMLElement>('[role^="menuitem"]') ?? []);
              const i = items.indexOf(document.activeElement as HTMLElement);
              const go = (n: number) => { e.preventDefault(); items[(n + items.length) % items.length]?.focus(); };
              if (e.key === 'ArrowDown') go(i + 1);
              else if (e.key === 'ArrowUp') go(i < 0 ? -1 : i - 1);
              else if (e.key === 'Home') go(0);
              else if (e.key === 'End') go(-1);
              else if (e.key === 'Tab') { e.preventDefault(); closePlusMenu(); }
            }}
            style={{ left: Math.min(plusMenu.x, window.innerWidth - 260), top: Math.min(plusMenu.y, window.innerHeight - 220) }}
            className="fixed z-[9999] w-[240px] bg-[#15151a] border border-white/10 rounded-lg shadow-2xl py-1 text-[11.5px]"
          >
            {!isCompact && (
              <>
                <button
                  role="menuitem"
                  tabIndex={-1}
                  onClick={() => {
                    closePlusMenu();
                    const newId = `${session.id}-term-${Date.now()}`;
                    setTerminals(prev => [...prev, { id: newId, title: nextTerminalNumber(prev), ownLogin: ownLoginDefault }]);
                    if (splitTerminals.length >= 2) {
                      setSplitTerminals(prev => [...prev, newId]);
                      setSplitRatios(prev => [...prev, 1]);
                    } else {
                      setSplitTerminals([activeTab, newId]);
                      setSplitRatios([1, 1]);
                    }
                    setActiveTab(newId);
                  }}
                  className="w-full flex items-center gap-2.5 px-3 py-1.5 hover:bg-white/[0.06] focus-visible:bg-white/[0.06] outline-none text-zinc-200 hover:text-white text-left"
                >
                  {splitOrientation === "h" ? <Columns size={13} className="text-primary/70" /> : <Rows size={13} className="text-primary/70" />}
                  <span className="flex-1">{splitTerminals.length >= 2 ? `Add pane (${splitTerminals.length + 1}-way split)` : "Split with new pane"}</span>
                </button>
                <button
                  role="menuitem"
                  tabIndex={-1}
                  onClick={() => {
                    setSplitOrientation(o => o === "h" ? "v" : "h");
                    closePlusMenu();
                  }}
                  className="w-full flex items-center gap-2.5 px-3 py-1.5 hover:bg-white/[0.06] focus-visible:bg-white/[0.06] outline-none text-zinc-300 hover:text-white text-left"
                >
                  {splitOrientation === "h" ? <Rows size={13} className="text-zinc-500" /> : <Columns size={13} className="text-zinc-500" />}
                  <span className="flex-1">Flip orientation ({splitOrientation === "h" ? "→ vertical" : "→ horizontal"})</span>
                </button>
                {splitTerminals.length >= 2 && (
                  <button
                    role="menuitem"
                    tabIndex={-1}
                    onClick={() => {
                      setSplitTerminals([]);
                      setSplitRatios([]);
                      closePlusMenu();
                    }}
                    className="w-full flex items-center gap-2.5 px-3 py-1.5 hover:bg-white/[0.06] focus-visible:bg-white/[0.06] outline-none text-zinc-300 hover:text-white text-left"
                  >
                    <X size={13} className="text-zinc-500" />
                    <span className="flex-1">Exit split (keep terminals)</span>
                  </button>
                )}
              </>
            )}
            {/* A terminal that logs in on its own, as a new connection to
                the server would (issue #230): once, or always for this
                server on this device. */}
            <>
                <button
                  role="menuitem"
                  tabIndex={-1}
                  onClick={() => {
                    closePlusMenu();
                    const newId = `${session.id}-term-${Date.now()}`;
                    setTerminals(prev => [...prev, { id: newId, title: nextTerminalNumber(prev), ownLogin: true }]);
                    setActiveTab(newId);
                  }}
                  className="w-full flex items-center gap-2.5 px-3 py-1.5 hover:bg-white/[0.06] focus-visible:bg-white/[0.06] outline-none text-zinc-200 hover:text-white text-left"
                >
                  <LogIn size={13} className="text-primary/70" />
                  <span className="flex-1">New terminal with its own login</span>
                </button>
                {session.serverUuid && (
                  <button
                    role="menuitemcheckbox"
                    aria-checked={ownLoginDefault}
                    tabIndex={-1}
                    onClick={() => {
                      const on = !ownLoginDefault;
                      writeOwnLogin(session.serverUuid, on);
                      setOwnLoginDefault(on);
                      closePlusMenu();
                    }}
                    title="Each new terminal logs in to the server again, as a new connection would, instead of sharing this tab's connection. For servers that tie rights to the login (CA Access Control, for one). Kept on this device."
                    className="w-full flex items-center gap-2.5 px-3 py-1.5 hover:bg-white/[0.06] focus-visible:bg-white/[0.06] outline-none text-zinc-300 hover:text-white text-left"
                  >
                    {ownLoginDefault ? <Check size={13} className="text-primary" /> : <span className="w-[13px] shrink-0" />}
                    <span className="flex-1">Always for this server</span>
                  </button>
                )}
            </>
            {/* Splitting THIS session's view with another server's tab
                lives on the session tab strip (right-click the tab).
                That way the picker is anchored to the tab it operates
                on and users don't have to know two entry points for the
                same feature. */}
          </div>
        </>,
        document.body
      )}

      {/* A terminal tab's menu: its name, edited in place. Enter keeps it
          (an empty one brings back the tab's first title), Esc goes back
          to the menu. */}
      {termMenu && terminals.some(t => t.id === termMenu.termId) && createPortal(
        <>
          <div
            className="fixed inset-0 z-[9998]"
            onClick={() => setTermMenu(null)}
            onContextMenu={(e) => { e.preventDefault(); setTermMenu(null); }}
          />
          <div
            style={{ left: Math.min(termMenu.x, window.innerWidth - 240), top: Math.min(termMenu.y, window.innerHeight - 80) }}
            className="fixed z-[9999] w-[220px] bg-[#15151a] border border-white/10 rounded-lg shadow-2xl py-1 text-[11.5px]"
          >
            {termRename === termMenu.termId ? (
              <RenameInput
                initialValue={terminals.find(t => t.id === termMenu.termId)?.title ?? ""}
                placeholder="Tab name (empty: the first one)"
                onCommit={(v) => { renameTerminal(termMenu.termId, v); setTermMenu(null); }}
                onCancel={() => setTermRename(null)}
              />
            ) : (
              <button
                onClick={() => setTermRename(termMenu.termId)}
                className="w-full flex items-center gap-2.5 px-3 py-1.5 hover:bg-white/[0.06] text-zinc-200 hover:text-white text-left"
              >
                <span className="flex-1">Rename tab</span>
              </button>
            )}
          </div>
        </>,
        document.body
      )}

      {dockMenu && createPortal(
        <>
          <div
            className="fixed inset-0 z-[9998]"
            onClick={() => setDockMenu(null)}
            onContextMenu={(e) => { e.preventDefault(); setDockMenu(null); }}
          />
          <div
            role="menu"
            aria-label="Panel position"
            style={{ left: Math.max(8, Math.min(dockMenu.x - 210, window.innerWidth - 218)), top: Math.min(dockMenu.y, window.innerHeight - 170) }}
            className="fixed z-[9999] w-[210px] bg-[#15151a] border border-white/10 rounded-lg shadow-2xl py-1 text-[11.5px]"
          >
            <div className="px-3 pt-1 pb-1.5 text-[9.5px] font-bold uppercase tracking-wider text-zinc-500">Panel position</div>
            {DOCK_OPTIONS.map(({ id, label, Icon }) => (
              <button
                key={id}
                role="menuitemradio"
                aria-checked={toolDock === id}
                onClick={() => { setToolDockPersisted(id); setToolFullPersisted(false); setDockMenu(null); }}
                className="w-full flex items-center gap-2.5 px-3 py-1.5 hover:bg-white/[0.06] text-zinc-200 hover:text-white text-left"
              >
                <Icon size={13} className={toolDock === id ? "text-primary" : "text-zinc-500"} />
                <span className="flex-1">{label}</span>
                {toolDock === id && <Check size={12} className="text-primary" />}
              </button>
            ))}
          </div>
        </>,
        document.body
      )}

      {/* Disconnection / auto-reconnect banner. Pinned to the top so it's
          visible whether the terminal or SFTP is in focus. The disabled
          overlay inside TerminalView / SftpWorkspace / TunnelsPanel does the
          heavy lifting of locking input out. */}
      {(status === 'disconnected' || reconnectAttempt > 0) && (
        (() => {
          const isAttempting = reconnectAttempt > 0 && status === 'connecting';
          const isWaiting = reconnectAttempt > 0 && nextReconnectAt !== null;
          const secs = <SecondsLeft until={nextReconnectAt ?? 0} />;
          const tone =
            isAttempting ? "bg-indigo-500/10 border-indigo-500/30 text-indigo-300" :
            isWaiting    ? "bg-amber-500/10 border-amber-500/30 text-amber-300" :
                           "bg-red-500/10 border-red-500/30 text-red-400";
          // The tooltip's heading has no seconds: a tooltip doesn't follow
          // the countdown.
          const heading =
            isAttempting ? `Reconnecting · attempt ${reconnectAttempt}` :
            isWaiting    ? `Auto-reconnect · attempt ${reconnectAttempt}` :
                           "Session disconnected";
          const shownHeading = isWaiting ? <>Auto-reconnect in {secs}s · attempt {reconnectAttempt}</> : heading;
          // A phone's banner stays one line: the short form of the heading
          // (cut with … if even that doesn't fit, never wrapped), no reason,
          // and icon buttons. The wide one is unchanged.
          const shortHeading =
            isAttempting ? `Reconnecting · #${reconnectAttempt}` :
            isWaiting    ? <>Retry in {secs}s · #{reconnectAttempt}</> :
                           "Disconnected";
          const btn = "h-8 min-w-8 sm:h-auto sm:min-h-6 px-2 sm:px-3 sm:py-1 rounded-md text-[10px] font-bold uppercase tracking-wider transition-colors flex items-center justify-center gap-1.5";
          return (
            <div className={`shrink-0 px-3 sm:px-4 py-1.5 sm:py-2 border-b flex items-center gap-2 sm:gap-3 animate-in fade-in ${tone}`}>
              <AlertTriangle size={14} className="shrink-0" />
              <div
                className="min-w-0 flex-1 truncate text-[11px] font-bold sm:uppercase sm:tracking-wider"
                title={disconnectReason ? `${heading} — ${disconnectReason}` : heading}
              >
                <span className="sm:hidden">{shortHeading}</span>
                <span className="hidden sm:inline">{shownHeading}</span>
                {disconnectReason && (
                  <span className="hidden sm:inline opacity-70 normal-case font-normal tracking-normal"> — {disconnectReason}</span>
                )}
              </div>
              <div className="shrink-0 flex items-center gap-1.5 sm:gap-2">
                {isWaiting && (
                  <button onClick={cancelReconnect} aria-label="Cancel" title="Stop reconnecting" className={`${btn} bg-white/5 text-zinc-300 hover:bg-white/10 hover:text-white`}>
                    <CircleSlash size={13} className="sm:hidden" /><span className="hidden sm:inline">Cancel</span>
                  </button>
                )}
                <button onClick={reconnect} aria-label={isWaiting ? "Now" : "Reconnect"} title={isWaiting ? "Reconnect now" : "Reconnect"} className={`${btn} bg-primary/10 text-primary hover:bg-primary/20`}>
                  <Play size={12} /><span className="hidden sm:inline">{isWaiting ? "Now" : "Reconnect"}</span>
                </button>
                <button onClick={onClose} aria-label="Close session" title="Close session" className={`${btn} bg-white/5 text-zinc-400 hover:bg-white/10 hover:text-white`}>
                  <X size={13} className="sm:hidden" /><span className="hidden sm:inline">Close</span>
                </button>
              </div>
            </div>
          );
        })()
      )}

      {/* Tab Content Split Pane. Side-by-side on roomy desktop; on narrow
          (`isCompact`) viewports the tool pane takes over the full width
          and the terminal is hidden — the tool tabs themselves act as the
          "back to terminal" affordance (clicking the active tool toggles
          it off). This avoids squeezing a usable terminal + tool into a
          mobile-sized window. Elsewhere the tool docks on the side the user
          picked (toolDock), unless they made it full screen (toolFull),
          which shows it alone the same way. */}
      <div
        ref={mainAreaRef}
        // Behind the push dialog: Tab can't reach the terminal under it.
        inert={shellSyncOpen ? "" : undefined}
        className={`flex-1 flex overflow-hidden relative bg-[#09090b] ${toolAlone ? "" : DOCK_FLEX[toolDock]}`}
      >
        {/* Left Panel: Active Terminals.
            With a tool open in a compact window, or made full screen, hide
            entirely so the tool fills the screen. Terminals stay mounted (no
            PTY teardown) — just CSS hidden so swapping back keeps the same
            shell session.

            Split mode: when `splitTerminals` holds 2+ ids and the
            viewport is roomy enough, we tile those terminals side-by-side
            (or stacked, per `splitOrientation`) with draggable dividers
            between adjacent panes. All OTHER terminals stay mounted but
            hidden — so switching a tab that isn't in the split preserves
            its scrollback. The tiles fill the whole left panel; the tool
            side panel continues to work exactly as before. */}
        <div className={`relative ${
          activeTool && toolAlone ? 'hidden'
            : dockTopBottom && !toolAlone ? 'w-full flex-1 min-h-0'
            : 'h-full flex-1 min-w-0'
        }`}>
          {splitTerminals.length >= 2 && !isCompact ? (
            <div className={`absolute inset-0 flex ${splitOrientation === "h" ? "flex-row" : "flex-col"}`}>
              {splitTerminals.map((termId, slotIdx) => {
                const t = terminals.find(x => x.id === termId);
                if (!t) return null;
                const isFocused = activeTab === t.id;
                const grow = splitRatios[slotIdx] ?? 1;
                const notLast = slotIdx < splitTerminals.length - 1;
                return (
                  <div
                    key={t.id}
                    style={{ flexGrow: grow, flexBasis: 0 }}
                    onMouseDown={() => setActiveTab(t.id)}
                    onTouchStart={() => setActiveTab(t.id)}
                    className={`relative min-w-0 min-h-0 border ${
                      isFocused
                        ? "border-primary/40 shadow-[inset_0_0_0_1px_rgba(var(--primary),0.15)]"
                        : "border-white/[0.03]"
                    }`}
                  >
                    <TerminalView
                      sessionId={session.id}
                      terminalId={t.id}
                      disabled={status !== 'connected'}
                      isActive={isFocused && !(activeTool && toolAlone)}
                      isActiveView={isActiveView}
                      focusSignal={focusTick}
                      containerExec={t.container ? { container: t.container.name, useSudo: t.container.useSudo } : undefined}
                      connectionEpoch={connectionEpoch}
                      serverId={session.serverId}
                      serverName={session.serverName}
                      ownLogin={t.ownLogin}
                      quickAuth={session.quickAuth}
                      onFolder={t.container ? undefined : onTerminalFolder}
                    />
                    {/* Per-pane detach affordance — pulls the pane out of
                        the split without killing the underlying terminal.
                        Positioned top-right so it doesn't fight the
                        divider (which lives on the pane's right/bottom
                        edge). Hidden until pane hover, tiny hit area. */}
                    <button
                      onClick={(e) => {
                        e.stopPropagation();
                        setSplitTerminals(prev => prev.filter(id => id !== t.id));
                        setSplitRatios(prev => prev.filter((_, i) => i !== slotIdx));
                        // If the focused pane left the split, hand focus
                        // to whichever pane is left (or fall back to the
                        // orphan terminal itself).
                        if (isFocused) {
                          const remaining = splitTerminals.filter(id => id !== t.id);
                          setActiveTab(remaining[0] ?? t.id);
                        }
                      }}
                      title="Remove this pane from the split (keeps the terminal)"
                      className="absolute top-1 right-1 z-30 h-5 w-5 rounded flex items-center justify-center bg-black/40 border border-white/10 text-zinc-400 hover:text-white hover:bg-black/60 opacity-0 hover:opacity-100 group-hover/pane:opacity-100 focus-visible:opacity-100 transition-opacity"
                    >
                      <X size={11} />
                    </button>
                    {/* Divider between this pane and the next. Rendered
                        only when there IS a next pane — otherwise a
                        trailing divider would float in dead space. */}
                    {notLast && (
                      <div
                        onMouseDown={(e) => {
                          e.preventDefault();
                          e.stopPropagation();
                          const container = e.currentTarget.parentElement?.parentElement;
                          if (!container) return;
                          const rect = container.getBoundingClientRect();
                          const total = splitOrientation === "h" ? rect.width : rect.height;
                          const startClient = splitOrientation === "h" ? e.clientX : e.clientY;
                          const sumGrow = splitRatios.reduce((a, b) => a + (b || 0), 0) || splitTerminals.length;
                          const pxPerUnit = total / sumGrow;
                          const startA = splitRatios[slotIdx] ?? 1;
                          const startB = splitRatios[slotIdx + 1] ?? 1;
                          const move = (ev: MouseEvent) => {
                            const client = splitOrientation === "h" ? ev.clientX : ev.clientY;
                            const deltaUnits = (client - startClient) / pxPerUnit;
                            // Redistribute between the two adjacent panes
                            // only — the rest of the row stays untouched.
                            // Clamp each so neither collapses below 0.3
                            // units (≈15% of the pair's original span).
                            const minPair = 0.3;
                            const combined = startA + startB;
                            const nextA = Math.max(minPair, Math.min(combined - minPair, startA + deltaUnits));
                            const nextB = combined - nextA;
                            setSplitRatios(prev => {
                              const copy = prev.slice();
                              copy[slotIdx] = nextA;
                              copy[slotIdx + 1] = nextB;
                              return copy;
                            });
                          };
                          const up = () => {
                            window.removeEventListener("mousemove", move);
                            window.removeEventListener("mouseup", up);
                          };
                          window.addEventListener("mousemove", move);
                          window.addEventListener("mouseup", up);
                        }}
                        onDoubleClick={(e) => {
                          e.stopPropagation();
                          // Reset all panes to equal share, not just this
                          // divider's pair — matches the "reset layout"
                          // idiom users expect from tmux/tiling WMs.
                          setSplitRatios(splitTerminals.map(() => 1));
                        }}
                        onContextMenu={(e) => {
                          e.preventDefault();
                          e.stopPropagation();
                          setSplitOrientation(o => o === "h" ? "v" : "h");
                        }}
                        title="Drag to resize · double-click resets · right-click flips orientation"
                        className={`absolute z-20 bg-white/5 hover:bg-primary/40 transition-colors ${
                          splitOrientation === "h"
                            ? "top-0 bottom-0 right-0 w-1 cursor-col-resize"
                            : "left-0 right-0 bottom-0 h-1 cursor-row-resize"
                        }`}
                      />
                    )}
                  </div>
                );
              })}
              {/* Off-screen host for terminals that are NOT in the split.
                  Keeps them mounted so their PTY buffer / scrollback
                  survives — critical for the "compare" workflow where the
                  user swaps a partner in and out of the tile. */}
              <div className="hidden">
                {terminals.filter(t => !splitTerminals.includes(t.id)).map(t => (
                  <TerminalView
                    key={t.id}
                    sessionId={session.id}
                    terminalId={t.id}
                    disabled={status !== 'connected'}
                    isActive={false}
                    containerExec={t.container ? { container: t.container.name, useSudo: t.container.useSudo } : undefined}
                    connectionEpoch={connectionEpoch}
                    serverId={session.serverId}
                    serverName={session.serverName}
                    ownLogin={t.ownLogin}
                    quickAuth={session.quickAuth}
                    onFolder={t.container ? undefined : onTerminalFolder}
                  />
                ))}
              </div>
            </div>
          ) : (
            terminals.map(t => (
              // A background tab stays mounted (its shell keeps running) but
              // inert, so Tab can't land in a terminal that isn't shown.
              <div key={t.id} inert={activeTab === t.id ? undefined : ""} className={`absolute inset-0 ${activeTab === t.id ? 'opacity-100 z-10' : 'opacity-0 z-0 pointer-events-none'}`}>
                <TerminalView
                  sessionId={session.id}
                  terminalId={t.id}
                  disabled={status !== 'connected'}
                  isActive={activeTab === t.id && !(activeTool && toolAlone)}
                  isActiveView={isActiveView}
                  focusSignal={focusTick}
                  containerExec={t.container ? { container: t.container.name, useSudo: t.container.useSudo } : undefined}
                  connectionEpoch={connectionEpoch}
                  serverId={session.serverId}
                  serverName={session.serverName}
                  ownLogin={t.ownLogin}
                  quickAuth={session.quickAuth}
                  onFolder={t.container ? undefined : onTerminalFolder}
                />
              </div>
            ))
          )}
        </div>

        {/* Resizable divider — only useful when both panes are visible. */}
        {activeTool && !toolAlone && (
          <div
            onMouseDown={startToolResize}
            className={`shrink-0 bg-white/5 hover:bg-primary/40 transition-colors ${
              dockTopBottom ? 'h-1 cursor-row-resize' : 'w-1 cursor-col-resize'
            }`}
            title="Drag to resize"
          />
        )}

        {/* Tool side panel. MirrorsPanel stays mounted even when the user
            pops over to SFTP / Ports / CMDS so its live status, log, and
            event subscriptions don't reset every tool switch — a mirror in
            initial-sync used to "look like it restarted" because the React
            tree was torn down. Other tools still conditional-render: they
            don't carry live state worth preserving across switches. */}
        <div
          ref={toolPanelRef}
          style={activeTool && !toolAlone
            ? (dockTopBottom
                ? { height: `${toolPanelHeight}px`, maxHeight: `calc(100% - ${MIN_TERMINAL_HEIGHT + 4}px)` }
                : { width: `${toolPanelWidth}px`, maxWidth: `calc(100% - ${MIN_TERMINAL_WIDTH + 4}px)` })
            : undefined}
          className={`${activeTool ? (toolAlone ? 'flex-1 min-w-0' : 'shrink-0') : 'hidden'} bg-[#121214]/95 flex flex-col ${dockTopBottom && !toolAlone ? 'w-full' : 'h-full'} overflow-hidden ${activeTool ? 'animate-in slide-in-from-right duration-300' : ''}`}
        >
          {activeTool === 'sftp' && (
            <div className="flex-1 flex flex-col overflow-hidden">
              <ToolTitleBar
                title="SFTP File Browser"
                shortTitle="SFTP"
                toggle={
                  <SepToggle
                    on={separateSftp}
                    onToggle={toggleSeparateSftp}
                    status={!separateSftp ? 'off' : sftpConnStatus === 'ready' ? 'ready' : sftpConnStatus === 'failed' ? 'failed' : 'pending'}
                    hint="SFTP runs on its own SSH connection, so large transfers don't slow the terminal down."
                    onReconnect={reconnectSftpConn}
                  />
                }
                follow={sftpView === 'files' ? { on: follow, onToggle: () => writeFollow(!follow) } : undefined}
                mirror={{
                  on: sftpView === 'mirror',
                  disabled: !session.serverId,
                  onToggle: () => setSftpView(v => v === 'mirror' ? 'files' : 'mirror'),
                }}
                extra={fullToggle}
                onClose={() => setActiveTool(null)}
              />
              <div className="flex-1 overflow-hidden relative">
                <SftpWorkspace
                  sessionId={session.id}
                  disabled={status !== 'connected'}
                  serverId={session.serverId}
                  profile={profile}
                  mirrorsConfig={parseObjectList(session.mirrors)}
                  view={sftpView}
                  follow={followFolder}
                />
              </div>
            </div>
          )}

          {activeTool === 'tunnels' && (
            <div className="flex-1 flex flex-col overflow-hidden">
              <ToolTitleBar
                title="Port Forwarding"
                shortTitle="Ports"
                toggle={
                  <SepToggle
                    on={separateFwd}
                    onToggle={toggleSeparateFwd}
                    status={!separateFwd ? 'off' : fwdConnStatus === 'ready' ? 'ready' : fwdConnStatus === 'failed' ? 'failed' : 'pending'}
                    hint="Port forwards run on their own SSH connection, so heavy traffic doesn't slow the terminal down."
                    onReconnect={reconnectFwdConn}
                  />
                }
                extra={fullToggle}
                onClose={() => setActiveTool(null)}
              />
              <div className="flex-1 overflow-hidden relative">
                <TunnelsPanel sessionId={session.id} serverId={session.serverId} disabled={status !== 'connected'} />
              </div>
            </div>
          )}

          {activeTool === 'cmds' && (
            <CmdsPanel
              activeTab={activeTab}
              onClose={() => setActiveTool(null)}
              headerActions={fullToggle}
              serverId={session.serverId}
              serverName={session.serverName}
            />
          )}

          {/* InfoPanel stays mounted across tool switches so its cached
              probe result survives a SFTP-and-back trip — the user
              explicitly asked for "cache while session is alive", and a
              fresh remount would re-fire the probe every tab swap. */}
          <div className={`flex-1 flex flex-col overflow-hidden ${activeTool === 'info' ? '' : 'hidden'}`}>
            <div className="h-10 px-4 flex items-center justify-between gap-3 border-b border-white/5 bg-white/5">
              <span className="text-[10px] font-bold uppercase tracking-wider text-zinc-400 truncate">Server Info</span>
              <div className="flex items-center gap-2 shrink-0">
                {fullToggle}
                <button onClick={() => setActiveTool(null)} aria-label="Close" className="text-zinc-500 hover:text-white transition-colors">
                  <X size={14} />
                </button>
              </div>
            </div>
            <InfoPanel
              sessionId={session.id}
              disabled={status !== 'connected'}
              visible={activeTool === 'info' && isActiveView}
              onOpenContainerTerminal={openContainerTerminal}
            />
          </div>
        </div>
      </div>

      {/* Push shell config — preview/confirm dialog. Scoped to this session
          (absolute, not a document.body portal) like promptOverlay above,
          since it only makes sense for the session whose toolbar button
          opened it. The session behind it is inert while it's open. */}
      {shellSyncOpen && (() => {
        const entries = shellSyncReport?.entries ?? [];
        const pushes = entries.filter((e) => e.action === "new" || e.action === "update" || e.action === "server-newer");
        const replaces = pushes.some((e) => e.action !== "new");
        const badge = (e: ShellSyncEntry): [string, string] => {
          switch (e.action) {
            case "new": return ["new", "text-emerald-400"];
            case "update": return ["update", "text-amber-400"];
            case "server-newer": return ["newer on the server", "text-rose-400"];
            case "skip-private": return [`not pushed: ${e.reason ?? "private"}`, "text-zinc-500"];
            case "skip-link": return ["a link there: left alone", "text-zinc-500"];
            case "skip-folder": return ["a folder there: left alone", "text-zinc-500"];
            default: return ["not a file there: left alone", "text-zinc-500"];
          }
        };
        const remoteShell = shellSyncShells?.remote_shell.split("/").pop() || "another shell";
        return (
          <div
            role="dialog"
            aria-modal="true"
            aria-labelledby={`shell-sync-title-${session.id}`}
            onKeyDown={(e) => {
              if (e.key === "Escape") {
                e.stopPropagation();
                closeShellSync();
              }
            }}
            className="absolute inset-0 z-50 flex items-start justify-center bg-black/70 backdrop-blur-sm p-4 sm:p-8 overflow-y-auto"
          >
            <div
              ref={shellSyncPanelRef}
              tabIndex={-1}
              className="max-w-xl w-full mt-6 sm:mt-12 bg-[#121215] border border-white/10 rounded-2xl shadow-2xl overflow-hidden outline-none"
            >
              <div className="px-5 py-4 border-b border-white/5 flex items-center justify-between gap-3">
                <div className="flex items-center gap-2 min-w-0">
                  <UploadCloud size={15} className="text-primary shrink-0" />
                  <h3 id={`shell-sync-title-${session.id}`} className="text-sm font-semibold text-zinc-100 truncate">
                    Push shell config → {session.serverName}
                  </h3>
                </div>
                <button
                  onClick={closeShellSync}
                  disabled={shellSyncPushing}
                  aria-label="Close"
                  className="shrink-0 text-zinc-500 hover:text-white transition-colors disabled:opacity-40"
                >
                  <X size={14} />
                </button>
              </div>

              <div className="px-5 py-4 space-y-3 max-h-[60vh] overflow-y-auto">
                {shellSyncBusy && !shellSyncReport && (
                  <div className="flex items-center gap-2 text-sm text-zinc-400 py-4">
                    <Loader2 size={14} className="animate-spin" /> Checking what would change…
                  </div>
                )}

                {shellSyncError && (
                  <div className="flex items-start gap-2 text-[12px] text-red-400 bg-red-500/10 border border-red-500/20 rounded-lg px-3 py-2.5">
                    <AlertTriangle size={14} className="shrink-0 mt-0.5" />
                    <span className="whitespace-pre-line break-words min-w-0">{shellSyncError}</span>
                  </div>
                )}

                {shellSyncShells?.shell_mismatch && !shellSyncResult && (
                  <div className="flex items-start gap-2 text-[12px] text-amber-300 bg-amber-500/10 border border-amber-500/20 rounded-lg px-3 py-2.5">
                    <AlertTriangle size={14} className="shrink-0 mt-0.5" />
                    <span>
                      This computer's shell is <strong>{shellSyncShells.local_shell}</strong>, but this account
                      logs into <strong>{remoteShell}</strong> on the server: files for {shellSyncShells.local_shell} are
                      only read there once the account's shell changes, or with Settings → Use my default shell.
                    </span>
                  </div>
                )}

                {shellSyncResult && (
                  <div className="space-y-2">
                    <p className="text-[12px] text-zinc-300">
                      Pushed {shellSyncResult.pushed} file{shellSyncResult.pushed === 1 ? "" : "s"}. These failed:
                    </p>
                    <div className="rounded-lg border border-red-500/20 divide-y divide-white/5 overflow-hidden">
                      {shellSyncResult.failed.map((f, i) => (
                        <div key={i} className="px-3 py-2 text-[12px]">
                          <div className="truncate text-zinc-200" title={f.remote}>{f.remote}</div>
                          <div className="text-red-400 break-words">{f.error}</div>
                        </div>
                      ))}
                    </div>
                    {shellSyncResult.backup && (
                      <p className="text-[11px] text-zinc-500">The files it replaced are in {shellSyncResult.backup} on the server.</p>
                    )}
                  </div>
                )}

                {shellSyncReport && !shellSyncResult && (
                  entries.length === 0 ? (
                    <p className="text-sm text-zinc-400 py-2">Everything is up to date: nothing to push.</p>
                  ) : (
                    <>
                      <p className="text-[11px] text-zinc-500">
                        {pushes.length === 0
                          ? "Nothing to push"
                          : `${pushes.length} file${pushes.length === 1 ? "" : "s"} · ${fmtBytes(shellSyncReport.total_bytes)}`}
                        {" "}· into {shellSyncReport.home} on the server
                      </p>
                      <div className="rounded-lg border border-white/10 divide-y divide-white/5 overflow-hidden">
                        {entries.map((e, i) => {
                          const [text, tone] = badge(e);
                          const sends = pushes.includes(e);
                          return (
                            <div key={i} className="px-3 py-2 text-[12px]">
                              <div className="flex items-center gap-2">
                                <span className={`flex-1 min-w-0 truncate ${sends ? "text-zinc-300" : "text-zinc-500"}`} title={`${e.local} → ${e.remote}`}>
                                  {e.remote}
                                </span>
                                {sends && <span className="shrink-0 text-zinc-500">{fmtBytes(e.size)}</span>}
                                <span className={`shrink-0 text-[10px] font-bold uppercase tracking-wide ${tone}`}>{text}</span>
                              </div>
                              {e.crlf && (
                                <p className="mt-0.5 text-[11px] text-amber-300">
                                  Windows line endings: a shell on the server reads the extra \r as part of each line.
                                </p>
                              )}
                            </div>
                          );
                        })}
                      </div>
                      {replaces && (
                        <p className="text-[11px] text-zinc-500">
                          A file it replaces is moved to ~/.submarine-trash on the server first and keeps its permissions.
                        </p>
                      )}
                    </>
                  )
                )}
              </div>

              <div className="px-5 py-3.5 border-t border-white/5 flex items-center justify-end gap-2">
                <button
                  onClick={closeShellSync}
                  disabled={shellSyncPushing}
                  className="h-8 px-3 rounded-lg text-xs font-semibold text-zinc-300 hover:bg-white/5 transition-colors disabled:opacity-40"
                >
                  {shellSyncResult ? "Close" : "Cancel"}
                </button>
                {!shellSyncResult && (
                  <button
                    onClick={confirmShellSync}
                    disabled={shellSyncBusy || pushes.length === 0}
                    className="h-8 px-3 rounded-lg text-xs font-semibold bg-primary/10 text-primary hover:bg-primary/20 transition-colors disabled:opacity-40 disabled:pointer-events-none flex items-center gap-1.5"
                  >
                    {shellSyncPushing ? <Loader2 size={13} className="animate-spin" /> : <UploadCloud size={13} />}
                    Push
                  </button>
                )}
              </div>
            </div>
          </div>
        );
      })()}
    </div>
  );

};

/// Memoised export so a re-render in DesktopApp (sessionStatuses changing
/// when ANY session connects/reconnects/drops) doesn't cascade through
/// every other session subtree. The default shallow comparator is fine
/// because the parent now hands stable closures (`addLog`, `onClose`,
/// `onStatusChange`) and `session` only changes identity when the user
/// adds or edits a row.
export const SessionView = memo(SessionViewImpl);
