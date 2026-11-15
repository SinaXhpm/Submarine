// What a reconnect is doing, as it happens: which step of the attempt it has
// reached (from the connect log the backend sends: reaching the server, the SSH
// handshake, logging in), the attempts so far, the time the current one has
// taken, and a bar under the banner that sweeps while an attempt runs and fills
// up to the next one. SessionView shows these in its reconnect banner and in
// the first connect screen.
//
// Every animation here changes only transforms or opacity (the compositor
// draws them) and lasts one attempt or one wait: none runs for ever. A server
// that stays down is still tried every few seconds for as long as it takes, so
// after the first attempts (ANIMATED_ATTEMPTS) the animations stop and only
// the numbers change; an endless animation kept a fifth of a CPU core busy
// (tailwind.config.js). A session that isn't on screen gets none. A system set
// to reduce motion gets no turning or sweeping, only fades and the countdown
// fill, so the banner still shows that it is at work.
import { useEffect, useState } from "react";
import { Check, Loader2, RotateCw, X } from "lucide-react";
import { tk, useT } from "../i18n";

export const ANIMATED_ATTEMPTS = 12;

export type ConnectStage = 0 | 1 | 2 | 3; // 0 reaching the server, 1 SSH handshake, 2 logging in, 3 logged in

// The connect log lines of initiate_connection (src-tauri/src/lib.rs) that
// mark each step. Matched loosely: a line that changes wording leaves the
// tracker on the step before it, nothing worse.
const STEP_LINES: [RegExp, ConnectStage][] = [
  [/^Authentication successful/i, 3],
  [/Authenticating user|^Attempting .*Authentication|^Logging in as/i, 2],
  [/^Starting SSH Handshake|TCP Connection established|tunnel established|opened tunnel to/i, 1],
];
const ATTEMPT_START = /^Initializing SSH connection process/i;

// The furthest step the latest attempt reached, read from the end of the log
// back to where that attempt began.
export function connectStage(logs: { msg: string }[]): ConnectStage {
  let stage: ConnectStage = 0;
  for (let i = logs.length - 1; i >= 0; i--) {
    const msg = logs[i].msg;
    for (const [re, s] of STEP_LINES) if (s > stage && re.test(msg)) stage = s;
    if (ATTEMPT_START.test(msg)) break;
  }
  return stage;
}

const STEPS = [tk("Reaching the server"), tk("Securing the connection"), tk("Logging in")];

const reducedMotion = () =>
  typeof window !== "undefined" && !!window.matchMedia?.("(prefers-reduced-motion: reduce)").matches;

// The three steps of an attempt. `running`: an attempt is under way, its
// current step pulses; otherwise the step the last attempt stopped at is marked
// as where it failed.
export function StageSteps({ stage, running, animate, compact = false }: { stage: ConnectStage; running: boolean; animate: boolean; compact?: boolean }) {
  const t = useT();
  return (
    <ol className="flex items-center gap-1.5 min-w-0" aria-label={t("Connection steps")}>
      {STEPS.map((label, i) => {
        const done = stage > i;
        const current = !done && stage === i;
        const failed = current && !running;
        const state = done ? t("{step}: done", { step: t(label) }) : failed ? t("{step}: failed", { step: t(label) }) : current ? t("{step}: in progress", { step: t(label) }) : t(label);
        const tone = done ? "text-emerald-300" : failed ? "text-red-300" : current ? "text-zinc-100" : "text-zinc-500";
        return (
          <li key={label} className="flex items-center gap-1.5 min-w-0" aria-label={state} title={state}>
            {i > 0 && (
              <span className="relative block h-px w-3 sm:w-5 shrink-0 overflow-hidden bg-white/10" aria-hidden="true">
                <span className={`absolute inset-0 origin-left bg-emerald-400/70 transition-transform duration-500 ${stage >= i ? "scale-x-100" : "scale-x-0"}`} />
              </span>
            )}
            <span className={`shrink-0 relative flex items-center justify-center w-4 h-4 rounded-full border transition-colors duration-300 ${
              done ? "border-emerald-400/60 bg-emerald-400/15" : failed ? "border-red-400/60 bg-red-500/15" : current ? "border-primary/70 bg-primary/15" : "border-white/15"
            }`} aria-hidden="true">
              {done ? <Check size={10} className="text-emerald-300" />
                : failed ? <X size={10} className="text-red-300" />
                : current ? <span className={`w-1.5 h-1.5 rounded-full bg-primary ${animate ? "animate-pulse-brief" : ""}`} />
                : null}
            </span>
            {/* On a phone only the current (or failed) step keeps its words. */}
            <span className={`truncate text-[11px] transition-colors duration-300 ${tone} ${compact && !current ? "hidden" : ""}`}>{t(label)}</span>
          </li>
        );
      })}
    </ol>
  );
}

// Attempts so far: the ones that failed, then the one running (pulsing) or the
// one coming next (hollow). Only the last few are drawn.
export function AttemptDots({ attempt, running, animate }: { attempt: number; running: boolean; animate: boolean }) {
  const t = useT();
  const failed = Math.max(0, attempt - 1);
  const shownFailed = Math.min(failed, 5);
  return (
    <span className="flex items-center gap-1 shrink-0" aria-label={t("Attempt {attempt}", { attempt })} title={t("Attempt {attempt}", { attempt })}>
      {failed > shownFailed && <span className="text-[10px] text-zinc-500 tabular-nums mr-0.5">+{failed - shownFailed}</span>}
      {Array.from({ length: shownFailed }, (_, i) => (
        <span key={i} className="w-1.5 h-1.5 rounded-full bg-red-400/70" aria-hidden="true" />
      ))}
      <span
        className={`w-2 h-2 rounded-full ${running ? `bg-primary ${animate ? "animate-pulse-brief" : ""}` : "border border-current opacity-60"}`}
        aria-hidden="true"
      />
    </span>
  );
}

// Time since it was put on screen, ticking once a second: how long this
// attempt has been at it. The caller keys it by attempt, so each attempt
// counts from 0:00.
export function Elapsed() {
  const [since] = useState(Date.now);
  const [now, setNow] = useState(since);
  useEffect(() => {
    const id = setInterval(() => setNow(Date.now()), 1000);
    return () => clearInterval(id);
  }, []);
  const s = Math.max(0, Math.floor((now - since) / 1000));
  return <span className="tabular-nums">{Math.floor(s / 60)}:{String(s % 60).padStart(2, "0")}</span>;
}

// The banner's icon: turning while an attempt runs (fading in and out when
// the system asks for less motion), the retry arrow while it waits for the
// next one.
export function ReconnectIcon({ running, animate, size = 14 }: { running: boolean; animate: boolean; size?: number }) {
  const spin = animate && !reducedMotion();
  return running
    ? <Loader2 size={size} className={`shrink-0 ${spin ? "animate-spin-attempt" : animate ? "animate-pulse-brief" : ""}`} aria-hidden="true" />
    : <RotateCw size={size} className="shrink-0" aria-hidden="true" />;
}

// A thin bar along the bottom edge. While an attempt runs a light sweeps along
// it (or, with less motion, the bar fades in and out); while waiting it fills
// up to the moment of the next attempt. Restarted by the caller's key for each
// attempt and each wait.
export function ReconnectBar({ running, until, animate, total }: { running: boolean; until: number | null; animate: boolean; total: number }) {
  if (running) {
    return (
      <div className="relative h-0.5 overflow-hidden bg-white/5" aria-hidden="true">
        {animate && !reducedMotion()
          ? <div className="absolute inset-y-0 left-0 w-1/3 bg-gradient-to-r from-transparent via-current to-transparent opacity-80 reconnect-sweep" />
          : <div className={`absolute inset-0 bg-current opacity-30 ${animate ? "reconnect-breathe" : ""}`} />}
      </div>
    );
  }
  if (until === null) return null;
  const left = Math.max(0, until - Date.now());
  const start = Math.min(1, Math.max(0, 1 - left / total));
  return (
    <div className="relative h-0.5 overflow-hidden bg-white/5" aria-hidden="true">
      <div
        className="absolute inset-0 origin-left bg-current opacity-60"
        style={animate
          ? { transform: `scaleX(${start})`, animation: `reconnect-fill ${left}ms linear forwards`, ["--fill-from" as string]: String(start) }
          : { transform: `scaleX(${start})` }}
      />
    </div>
  );
}
