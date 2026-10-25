import { useEffect, useRef, useState, type ReactNode } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { Signal, SignalHigh, SignalLow, SignalMedium, SignalZero, X } from "lucide-react";
import { SwitchRow } from "./FormRows";
import { RdpShareFields, RdpSpeedFields } from "./RdpOptionFields";
import { VncSpeedFields } from "./VncOptionFields";
import { formatBytes, formatRate, GOOD_BELOW_MS, LINK_WORDS, linkQuality, WEAK_FROM_MS, type LinkQuality, type LinkStats } from "../util/screenLink";
import type { RdpOptions } from "../util/rdpOptions";
import type { VncOptions } from "../util/vncOptions";

// The tools in the bar of a remote screen tab (the built-in RDP client, VNC):
// how the link is doing, the speed and quality, and for RDP what of this
// computer's files the server gets. Each opens a panel over the screen; what
// is changed there is kept for the server on this device, as the server form
// keeps it.

/** What the session now open shares (rdp_builtin_open says, with the connection). */
export type SharedNow = { name: string; path: string; readOnly: boolean };

const QUALITY_LOOK: Record<LinkQuality, { icon: typeof Signal; colour: string }> = {
  good: { icon: SignalHigh, colour: "text-emerald-400" },
  fair: { icon: SignalMedium, colour: "text-amber-300" },
  weak: { icon: SignalLow, colour: "text-red-400" },
  silent: { icon: SignalZero, colour: "text-red-400" },
  unknown: { icon: Signal, colour: "text-zinc-500" },
};

const QUALITY_SAYS: Record<LinkQuality, string> = {
  good: "The server answers quickly.",
  fair: "Clicks and keys may show a short wait.",
  weak: "Clicks and keys show a wait. Less data may help.",
  silent: "The server isn't answering. The link may be down, or very slow.",
  unknown: "Measuring…",
};

/** The connection mark: the link's quality, and its delay where there's room. */
export function LinkMark({ link, open, onClick }: { link: LinkStats | null; open: boolean; onClick: () => void }) {
  const q = linkQuality(link);
  const { icon: Icon, colour } = QUALITY_LOOK[q];
  const delay = q !== "silent" && link?.rttMs != null ? `${link.rttMs} ms` : "";
  const said = `Connection: ${LINK_WORDS[q]}${delay ? `, ${delay} round trip` : ""}`;
  return (
    <button
      onClick={onClick}
      aria-expanded={open}
      aria-label={said}
      title={`${said}. Details`}
      data-link-quality={q}
      className={`h-7 px-2 flex items-center gap-1.5 rounded-md text-[11px] font-semibold transition-colors ${open ? "bg-primary/15" : "bg-white/5 hover:bg-white/10"}`}
    >
      <Icon size={13} className={colour} aria-hidden="true" />
      <span className={`hidden [@container(min-width:560px)]:inline ${colour}`}>{LINK_WORDS[q]}</span>
      {delay && <span className="hidden [@container(min-width:700px)]:inline text-zinc-500 font-medium tabular-nums">{delay}</span>}
    </button>
  );
}

/** A panel over the desktop, under the tab's bar; a click beside it or Esc closes it. */
export function ToolPanel({ title, onClose, children }: { title: string; onClose: () => void; children: ReactNode }) {
  const box = useRef<HTMLDivElement | null>(null);
  // The keys go to the panel now, not to the server.
  useEffect(() => { box.current?.focus(); }, []);
  return (
    <div className="absolute inset-x-0 top-9 bottom-0 z-30">
      <div className="absolute inset-0" onMouseDown={onClose} aria-hidden="true" />
      <div
        ref={box}
        role="dialog"
        aria-label={title}
        tabIndex={-1}
        onKeyDown={(e) => { if (e.key === "Escape") { e.preventDefault(); onClose(); } }}
        className="absolute top-2 right-2 w-[min(380px,calc(100%-16px))] max-h-[calc(100%-16px)] overflow-y-auto overscroll-contain custom-scrollbar rounded-xl border border-white/10 bg-[#141417] shadow-2xl p-3 space-y-3 outline-none"
      >
        <div className="flex items-center justify-between gap-2">
          <h3 className="text-[12px] font-bold text-zinc-200">{title}</h3>
          <button onClick={onClose} aria-label="Close" className="h-6 w-6 flex items-center justify-center rounded-md text-zinc-500 hover:text-zinc-200 hover:bg-white/5">
            <X size={12} />
          </button>
        </div>
        {children}
      </div>
    </div>
  );
}

// A change the session now open hasn't got: it takes a new connection.
function ReconnectBar({ onReconnect }: { onReconnect: () => void }) {
  return (
    <div role="status" className="flex items-center gap-2 p-2 rounded-lg border border-primary/25 bg-primary/5 text-[11px] leading-snug text-zinc-300">
      <span className="flex-1 min-w-0">Some of this applies when the tab connects again.</span>
      <button onClick={onReconnect} className="shrink-0 h-7 px-2.5 rounded-md bg-primary/15 text-primary hover:bg-primary/25 font-semibold transition-colors">
        Reconnect now
      </button>
    </div>
  );
}

const KEPT = <p className="text-[10.5px] text-zinc-600 leading-snug">Kept for this server on this device.</p>;

function Figure({ label, value }: { label: string; value: string }) {
  return (
    <div className="flex items-baseline justify-between gap-3 text-[11.5px]">
      <span className="text-zinc-500">{label}</span>
      <span className="text-zinc-200 font-semibold tabular-nums text-right">{value}</span>
    </div>
  );
}

export function LinkPanel({ link, onLessData }: { link: LinkStats | null; onLessData: () => void }) {
  const q = linkQuality(link);
  const { icon: Icon, colour } = QUALITY_LOOK[q];
  const says = q === "unknown" && link && !link.canMeasure ? "This server doesn't answer the question the delay is measured with." : QUALITY_SAYS[q];
  const delay = link?.rttMs != null ? `${link.rttMs} ms` : link && !link.canMeasure ? "Can't be measured" : "Measuring…";
  return (
    <>
      <div className="flex items-start gap-2.5">
        <Icon size={18} className={`shrink-0 mt-0.5 ${colour}`} aria-hidden="true" />
        <div className="min-w-0">
          <div className={`text-[13px] font-bold ${colour}`}>{q === "unknown" ? "Connection" : `${LINK_WORDS[q]} connection`}</div>
          <p className="text-[11px] text-zinc-400 leading-snug">
            {q === "silent" && link ? `No answer for ${Math.round(link.waitingMs / 1000)} s. ` : ""}
            {says}
          </p>
        </div>
      </div>
      <div className="space-y-1.5 p-2.5 rounded-lg bg-[#1a1a1e] border border-white/5">
        <Figure label="Delay (there and back)" value={delay} />
        <Figure label="Receiving" value={link ? formatRate(link.inRate) : "–"} />
        <Figure label="Sending" value={link ? formatRate(link.outRate) : "–"} />
        <Figure label="This session" value={link ? `${formatBytes(link.bytesIn)} in, ${formatBytes(link.bytesOut)} out` : "–"} />
      </div>
      {link?.paused && <p className="text-[11px] text-zinc-400">The picture is paused while the tab is hidden.</p>}
      {(q === "weak" || q === "silent") && (
        <button
          onClick={onLessData}
          className="w-full h-8 rounded-lg bg-primary/15 text-primary hover:bg-primary/25 text-[11.5px] font-semibold transition-colors"
        >
          Use less data…
        </button>
      )}
      <p className="text-[10.5px] text-zinc-600 leading-snug">
        The delay is how long the server takes to answer, through everything on the way (SSH servers, VPNs). Under {GOOD_BELOW_MS} ms is good; from {WEAK_FROM_MS} ms on, every click shows the wait.
      </p>
    </>
  );
}

// `shared`: what the session now open shares; undefined while none is open.
export function FilesPanel({ shared, o, set, drives, reconnect }: {
  shared: SharedNow | null | undefined;
  o: RdpOptions;
  set: (patch: Partial<RdpOptions>) => void;
  drives: string[];
  reconnect: (() => void) | null;
}) {
  return (
    <>
      {shared !== undefined && (
        <div className="p-2.5 rounded-lg bg-[#1a1a1e] border border-white/5 text-[11.5px] leading-snug">
          <div className="text-zinc-500">Shared now</div>
          {shared ? (
            <div className="text-zinc-200 break-all">
              <span className="font-mono">\\tsclient\{shared.name}</span>: {shared.path}{shared.readOnly ? ", read only" : ""}
            </div>
          ) : (
            <div className="text-zinc-300">Nothing of this computer's.</div>
          )}
        </div>
      )}
      <RdpShareFields o={o} set={set} drives={drives} />
      {reconnect && <ReconnectBar onReconnect={reconnect} />}
      {KEPT}
    </>
  );
}

export function RdpSettingsPanel({ o, set, reconnect }: { o: RdpOptions; set: (patch: Partial<RdpOptions>) => void; reconnect: (() => void) | null }) {
  return (
    <>
      <RdpSpeedFields o={o} set={set} />
      <SwitchRow
        label="Share the clipboard"
        hint="Text you copy on the server comes to this device; this device's goes to the server when you paste it there."
        checked={o.clipboard}
        onChange={(clipboard) => set({ clipboard })}
      />
      {reconnect && <ReconnectBar onReconnect={reconnect} />}
      {KEPT}
    </>
  );
}

export function VncSettingsPanel({ o, set, reconnect }: { o: VncOptions; set: (patch: Partial<VncOptions>) => void; reconnect: (() => void) | null }) {
  return (
    <>
      <VncSpeedFields o={o} set={set} />
      <SwitchRow
        label="Share the clipboard"
        hint="Text you copy on the server comes to this device; this device's goes to the server when you paste it there. Plain ASCII text only, such as links and commands."
        checked={o.clipboard}
        onChange={(clipboard) => set({ clipboard })}
      />
      {reconnect && <ReconnectBar onReconnect={reconnect} />}
      {KEPT}
    </>
  );
}

// How long the tab is hidden before the picture pauses: a quick look at
// another tab changes nothing.
const PAUSE_AFTER_MS = 3000;

/** Whether the page is on screen: not minimised or behind another window's
 *  tab, and on a phone, the app not in the background. */
export function usePageShown(): boolean {
  const [pageShown, setPageShown] = useState(() => document.visibilityState !== "hidden");
  useEffect(() => {
    const onChange = () => setPageShown(document.visibilityState !== "hidden");
    document.addEventListener("visibilitychange", onChange);
    // A phone's app in the background: its WebView still says it is visible,
    // so the backend tells (lib.rs, from Android's onPause and onResume).
    const off = listen<boolean>("app-shown", (e) => setPageShown(!!e.payload));
    return () => {
      document.removeEventListener("visibilitychange", onChange);
      off.then((f) => f());
    };
  }, []);
  return pageShown;
}

/** While `pause` holds for PAUSE_AFTER_MS, the session is told the tab is
 *  hidden (`command`, shown: false); as soon as it doesn't, that it is shown. */
export function usePauseWhileHidden(pause: boolean, command: string, sessionId: string) {
  const pauseSent = useRef(false);
  useEffect(() => {
    if (!pause) {
      if (pauseSent.current) {
        pauseSent.current = false;
        invoke(command, { sessionId, shown: true }).catch(() => {});
      }
      return;
    }
    const timer = window.setTimeout(() => {
      pauseSent.current = true;
      invoke(command, { sessionId, shown: false }).catch(() => {});
    }, PAUSE_AFTER_MS);
    return () => window.clearTimeout(timer);
  }, [pause, command, sessionId]);
}
