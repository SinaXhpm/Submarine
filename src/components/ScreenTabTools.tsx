import { useEffect, useRef, useState, type ReactNode } from "react";
import { invoke } from "@tauri-apps/api/core";
import { Signal, SignalHigh, SignalLow, SignalMedium, SignalZero, X } from "lucide-react";
import { SwitchRow } from "./FormRows";
import { RdpShareFields, RdpSpeedFields } from "./RdpOptionFields";
import { VncSpeedFields } from "./VncOptionFields";
import { formatBytes, formatRate, GOOD_BELOW_MS, LINK_WORDS, linkQuality, WEAK_FROM_MS, type LinkQuality, type LinkStats } from "../util/screenLink";
import type { RdpOptions } from "../util/rdpOptions";
import type { VncOptions } from "../util/vncOptions";
import { t, tk, tx, useT } from "../i18n";

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
  good: tk("The server answers quickly."),
  fair: tk("Clicks and keys may show a short wait."),
  weak: tk("Clicks and keys show a wait. Less data may help."),
  silent: tk("The server isn't answering. The link may be down, or very slow."),
  unknown: tk("Measuring…"),
};

/** The connection mark: the link's quality, and its delay where there's room. */
export function LinkMark({ link, open, onClick }: { link: LinkStats | null; open: boolean; onClick: () => void }) {
  const t = useT();
  const q = linkQuality(link);
  const { icon: Icon, colour } = QUALITY_LOOK[q];
  const delay = q !== "silent" && link?.rttMs != null ? t("{ms} ms", { ms: link.rttMs }) : "";
  const quality = t(LINK_WORDS[q]);
  const said = delay ? t("Connection: {quality}, {delay} round trip", { quality, delay }) : t("Connection: {quality}", { quality });
  return (
    <button
      onClick={onClick}
      aria-expanded={open}
      aria-label={said}
      title={delay ? t("Connection: {quality}, {delay} round trip. Details", { quality, delay }) : t("Connection: {quality}. Details", { quality })}
      data-link-quality={q}
      className={`h-7 px-2 flex items-center gap-1.5 rounded-md text-[11px] font-semibold transition-colors ${open ? "bg-primary/15" : "bg-white/5 hover:bg-white/10"}`}
    >
      <Icon size={13} className={colour} aria-hidden="true" />
      <span className={`hidden [@container(min-width:560px)]:inline ${colour}`}>{quality}</span>
      {delay && <span className="hidden [@container(min-width:700px)]:inline text-zinc-500 font-medium tabular-nums">{delay}</span>}
    </button>
  );
}

/** A panel over the desktop, under the tab's bar; a click beside it or Esc closes it. */
export function ToolPanel({ title, onClose, children }: { title: string; onClose: () => void; children: ReactNode }) {
  const t = useT();
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
          <button onClick={onClose} aria-label={t("Close")} className="h-6 w-6 flex items-center justify-center rounded-md text-zinc-500 hover:text-zinc-200 hover:bg-white/5">
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
  const t = useT();
  return (
    <div role="status" className="flex items-center gap-2 p-2 rounded-lg border border-primary/25 bg-primary/5 text-[11px] leading-snug text-zinc-300">
      <span className="flex-1 min-w-0">{t("Some of this applies when the tab connects again.")}</span>
      <button onClick={onReconnect} className="shrink-0 h-7 px-2.5 rounded-md bg-primary/15 text-primary hover:bg-primary/25 font-semibold transition-colors">
        {t("Reconnect now")}
      </button>
    </div>
  );
}

// Made where it is shown, so a language change reaches it.
const kept = () => <p className="text-[10.5px] text-zinc-600 leading-snug">{t("Kept for this server on this device.")}</p>;

function Figure({ label, value }: { label: string; value: string }) {
  return (
    <div className="flex items-baseline justify-between gap-3 text-[11.5px]">
      <span className="text-zinc-500">{label}</span>
      <span className="text-zinc-200 font-semibold tabular-nums text-right">{value}</span>
    </div>
  );
}

export function LinkPanel({ link, onLessData }: { link: LinkStats | null; onLessData: () => void }) {
  const t = useT();
  const q = linkQuality(link);
  const { icon: Icon, colour } = QUALITY_LOOK[q];
  const says = q === "unknown" && link && !link.canMeasure ? t("This server doesn't answer the question the delay is measured with.") : t(QUALITY_SAYS[q]);
  const delay = link?.rttMs != null ? t("{ms} ms", { ms: link.rttMs }) : link && !link.canMeasure ? t("Can't be measured") : t("Measuring…");
  return (
    <>
      <div className="flex items-start gap-2.5">
        <Icon size={18} className={`shrink-0 mt-0.5 ${colour}`} aria-hidden="true" />
        <div className="min-w-0">
          <div className={`text-[13px] font-bold ${colour}`}>{q === "unknown" ? t("Connection") : t("{quality} connection", { quality: t(LINK_WORDS[q]) })}</div>
          <p className="text-[11px] text-zinc-400 leading-snug">
            {q === "silent" && link ? t("No answer for {seconds} s.", { seconds: Math.round(link.waitingMs / 1000) }) + " " : ""}
            {says}
          </p>
        </div>
      </div>
      <div className="space-y-1.5 p-2.5 rounded-lg bg-[#1a1a1e] border border-white/5">
        <Figure label={t("Delay (there and back)")} value={delay} />
        <Figure label={t("Receiving")} value={link ? formatRate(link.inRate) : "–"} />
        <Figure label={t("Sending")} value={link ? formatRate(link.outRate) : "–"} />
        <Figure label={t("This session")} value={link ? t("{received} in, {sent} out", { received: formatBytes(link.bytesIn), sent: formatBytes(link.bytesOut) }) : "–"} />
      </div>
      {link?.paused && <p className="text-[11px] text-zinc-400">{t("The picture is paused while the tab is hidden.")}</p>}
      {(q === "weak" || q === "silent") && (
        <button
          onClick={onLessData}
          className="w-full h-8 rounded-lg bg-primary/15 text-primary hover:bg-primary/25 text-[11.5px] font-semibold transition-colors"
        >
          {t("Use less data…")}
        </button>
      )}
      <p className="text-[10.5px] text-zinc-600 leading-snug">
        {t("The delay is how long the server takes to answer, through everything on the way (SSH servers, VPNs). Under {good} ms is good; from {weak} ms on, every click shows the wait.", { good: GOOD_BELOW_MS, weak: WEAK_FROM_MS })}
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
  const t = useT();
  const share = shared ? <span className="font-mono">\\tsclient\{shared.name}</span> : null;
  return (
    <>
      {shared !== undefined && (
        <div className="p-2.5 rounded-lg bg-[#1a1a1e] border border-white/5 text-[11.5px] leading-snug">
          <div className="text-zinc-500">{t("Shared now")}</div>
          {shared ? (
            <div className="text-zinc-200 break-all">
              {shared.readOnly
                ? tx("{share}: {path}, read only", { share, path: shared.path })
                : tx("{share}: {path}", { share, path: shared.path })}
            </div>
          ) : (
            <div className="text-zinc-300">{t("Nothing of this computer's.")}</div>
          )}
        </div>
      )}
      <RdpShareFields o={o} set={set} drives={drives} />
      {reconnect && <ReconnectBar onReconnect={reconnect} />}
      {kept()}
    </>
  );
}

export function RdpSettingsPanel({ o, set, reconnect }: { o: RdpOptions; set: (patch: Partial<RdpOptions>) => void; reconnect: (() => void) | null }) {
  const t = useT();
  return (
    <>
      <RdpSpeedFields o={o} set={set} />
      <SwitchRow
        label={t("Share the clipboard")}
        hint={t("Text you copy on the server comes to this device, by itself while the tab is in front; this device's goes to the server when you paste it there.")}
        checked={o.clipboard}
        onChange={(clipboard) => set({ clipboard })}
      />
      {reconnect && <ReconnectBar onReconnect={reconnect} />}
      {kept()}
    </>
  );
}

export function VncSettingsPanel({ o, set, reconnect }: { o: VncOptions; set: (patch: Partial<VncOptions>) => void; reconnect: (() => void) | null }) {
  const t = useT();
  return (
    <>
      <VncSpeedFields o={o} set={set} />
      <SwitchRow
        label={t("Share the clipboard")}
        hint={t("Text you copy on the server comes to this device, by itself while the tab is in front; this device's goes to the server when you paste it there. Plain ASCII text only, such as links and commands.")}
        checked={o.clipboard}
        onChange={(clipboard) => set({ clipboard })}
      />
      {reconnect && <ReconnectBar onReconnect={reconnect} />}
      {kept()}
    </>
  );
}

// How long the tab is hidden before the picture pauses: a quick look at
// another tab changes nothing.
const PAUSE_AFTER_MS = 3000;

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
