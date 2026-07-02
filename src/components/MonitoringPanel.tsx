import { useEffect, useMemo, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import {
  Activity, Pause, Play, Trash2, Plus, X, Wifi, WifiOff, AlertTriangle,
  RefreshCw, Settings2, FileCode2, Settings as SettingsIcon, Bell, BellOff, Volume2,
} from "lucide-react";
import Sparkline from "./Sparkline";
import { useConfirm } from "../ui/confirm";

// ---------- shared types ------------------------------------------------------

interface CustomMetric {
  id: string;
  name: string;
  command: string;
  parse: "number" | "regex" | "text";
  regex?: string;
  display: "sparkline" | "big_number" | "text";
  unit?: string;
}

interface MonitorRow {
  node_id: number;
  name: string;
  host: string;
  port: number;
  enabled_metrics: string[];
  custom_metrics: CustomMetric[];
  paused: boolean;
  connected: boolean;
  last_error?: string | null;
  last_sample_ts?: number | null;
  consecutive_failures?: number;
  outage_since_ms?: number | null;
}

interface Sample {
  node_id: number;
  ts: number;
  values: Record<string, number>;
  errors: Record<string, string>;
  texts: Record<string, string>;
}

interface OutageEvent {
  node_id: number;
  node_name: string;
  since_ms: number;
  consecutive_failures: number;
  last_error: string;
  notify: boolean;
  beep: boolean;
}
interface RecoveredEvent {
  node_id: number;
  node_name: string;
  since_ms: number;
  duration_ms: number;
  notify: boolean;
}

interface MonitorSettings {
  interval_secs: number;
  connect_timeout_secs: number;
  poll_timeout_secs: number;
  outage_threshold: number;
  notify_on_outage: boolean;
  beep_on_outage: boolean;
  beep_cooldown_secs: number;
}

// ---------- beep (anti-spam audible alert) -----------------------------------

// Lazy-init a single shared AudioContext. Web Audio synthesises the tone
// in-process so we don't ship any audio assets — works the same on Windows,
// macOS, and Linux because there's no OS-specific sound API involved. The
// alternative (calling out to a platform-specific tool like
// SystemSounds.Beep on Windows or `afplay` on macOS) would need separate
// per-platform branches in Rust.
let sharedAudioCtx: AudioContext | null = null;
const beepNow = () => {
  try {
    if (!sharedAudioCtx) {
      // Some browsers expose AudioContext only on the prefixed name in
      // older versions — fall back if needed.
      const Ctor: any = (window as any).AudioContext || (window as any).webkitAudioContext;
      if (!Ctor) return;
      sharedAudioCtx = new Ctor();
    }
    const ctx = sharedAudioCtx!;
    // Most browsers suspend the context until a user gesture; resume()
    // returns a Promise so we can fire the tone right after.
    void ctx.resume?.();
    const t = ctx.currentTime;
    const osc = ctx.createOscillator();
    const gain = ctx.createGain();
    osc.connect(gain);
    gain.connect(ctx.destination);
    osc.type = "sine";
    osc.frequency.value = 880; // A5 — distinct from any natural ambient hum
    // Short attack/release envelope so it sounds like a notification ping
    // rather than a continuous tone.
    gain.gain.setValueAtTime(0.0, t);
    gain.gain.linearRampToValueAtTime(0.18, t + 0.02);
    gain.gain.linearRampToValueAtTime(0.0, t + 0.28);
    osc.start(t);
    osc.stop(t + 0.3);
  } catch {
    // Swallow — failure to beep should never break the monitoring UI.
  }
};

const HISTORY = 60;

// Built-in metric metadata. Custom metrics carry their own metadata.
// Net is reported in **bits** (Mbps etc) because that's how network gear
// is universally rated — link speeds, ISP plans, switch port labels are
// all "Mbps", not MB/s. Showing the same units the user sees on their
// router makes the value immediately comparable.
const BUILTINS: Record<string, { label: string; unit: string; is_pct: boolean; color: string }> = {
  cpu:     { label: "CPU",       unit: "%",    is_pct: true,  color: "rgb(96,165,250)" },
  mem:     { label: "Memory",    unit: "%",    is_pct: true,  color: "rgb(129,140,248)" },
  swap:    { label: "Swap",      unit: "%",    is_pct: true,  color: "rgb(192,132,252)" },
  disk:    { label: "Disk /",    unit: "%",    is_pct: true,  color: "rgb(251,191,36)" },
  load:    { label: "Load 1m",   unit: "",     is_pct: false, color: "rgb(244,114,182)" },
  net_in:  { label: "Net In",    unit: "Mbps", is_pct: false, color: "rgb(34,211,238)" },
  net_out: { label: "Net Out",   unit: "Mbps", is_pct: false, color: "rgb(251,113,133)" },
};

const TOGGLE_KEYS: { key: string; sub_keys: string[]; label: string }[] = [
  { key: "cpu",  sub_keys: ["cpu"],            label: "CPU" },
  { key: "mem",  sub_keys: ["mem"],            label: "Memory" },
  { key: "swap", sub_keys: ["swap"],           label: "Swap" },
  { key: "disk", sub_keys: ["disk"],           label: "Disk /" },
  { key: "load", sub_keys: ["load"],           label: "Load 1m" },
  { key: "net",  sub_keys: ["net_in", "net_out"], label: "Network" },
];

const CUSTOM_COLOR = "rgb(251,191,36)";

// ---------- formatters --------------------------------------------------------

// Convert bytes/sec → bits/sec then pick the most appropriate decimal unit.
// We use SI multipliers (1000) for network because Mbps in the wild means
// megabits per second on the wire, not mebibits. Mbps → bps → Kbps → Gbps
// keeps numbers in a readable range across 7+ orders of magnitude.
const formatBitsPerSec = (bytesPerSec: number) => {
  if (!isFinite(bytesPerSec) || bytesPerSec < 0) return "—";
  const bits = bytesPerSec * 8;
  if (bits >= 1e9) return `${(bits / 1e9).toFixed(2)} Gbps`;
  if (bits >= 1e6) return `${(bits / 1e6).toFixed(2)} Mbps`;
  if (bits >= 1e3) return `${(bits / 1e3).toFixed(1)} Kbps`;
  return `${Math.round(bits)} bps`;
};
const formatBuiltin = (key: string, v: number | undefined) => {
  if (v === undefined || !isFinite(v)) return "—";
  if (key === "net_in" || key === "net_out") return formatBitsPerSec(v);
  if (BUILTINS[key]?.is_pct) return `${v.toFixed(1)}%`;
  return v.toFixed(2);
};
const formatCustomNumber = (v: number | undefined, unit?: string) => {
  if (v === undefined || !isFinite(v)) return "—";
  const num = Math.abs(v) >= 100 ? v.toFixed(0)
            : Math.abs(v) >= 1   ? v.toFixed(2)
                                 : v.toFixed(3);
  return unit ? `${num} ${unit}` : num;
};
const timeAgo = (ts?: number | null) => {
  if (!ts) return "never";
  // Floor (not round) so the display advances cleanly: 0s shows "just now",
  // 1s shows "1s ago", 2s shows "2s ago" — no jitter at the .5 boundary.
  // Previously this clamped the first 5 seconds to "just now" which made
  // the counter look frozen because polling fires every 5s.
  const dt = Math.max(0, Math.floor((Date.now() - ts) / 1000));
  if (dt === 0) return "just now";
  if (dt < 60) return `${dt}s ago`;
  if (dt < 3600) return `${Math.floor(dt / 60)}m ago`;
  return `${Math.floor(dt / 3600)}h ago`;
};
const formatDuration = (ms: number) => {
  const s = Math.floor(ms / 1000);
  if (s < 60) return `${s}s`;
  if (s < 3600) return `${Math.floor(s / 60)}m ${s % 60}s`;
  const h = Math.floor(s / 3600);
  const m = Math.floor((s % 3600) / 60);
  return `${h}h ${m}m`;
};
const randomId = () => Math.random().toString(36).slice(2, 10);

// ---------- component ---------------------------------------------------------

interface Props {
  servers: any[];
  refreshServers: () => void;
  /// Optional hook to push lines into the global Logs tab. When provided,
  /// every outage / recovered event is recorded there too so the user has
  /// a persistent history beyond the transient toast.
  addLog?: (msg: string, type: "info" | "success" | "warn" | "error") => void;
}

const MonitoringPanel = ({ servers, addLog }: Props) => {
  const confirm = useConfirm();
  const [rows, setRows] = useState<MonitorRow[]>([]);
  const [addOpen, setAddOpen] = useState(false);
  const [configFor, setConfigFor] = useState<number | null>(null);
  const [settingsOpen, setSettingsOpen] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  // Transient toast for outage / recovered events. Replaces previous on new
  // event so a single dismiss timer is enough.
  const [toast, setToast] = useState<{ msg: string; tone: "warn" | "ok" | "err" } | null>(null);
  const toastTimerRef = useRef<number | null>(null);
  const showToast = (msg: string, tone: "warn" | "ok" | "err") => {
    setToast({ msg, tone });
    if (toastTimerRef.current) window.clearTimeout(toastTimerRef.current);
    toastTimerRef.current = window.setTimeout(() => setToast(null), 4500);
  };

  // Ring buffers / latest per node.
  const [history, setHistory] = useState<Record<number, Record<string, number[]>>>({});
  const [latest, setLatest] = useState<Record<number, Sample>>({});

  // Cached settings for client-side gating (beep cooldown lives here, not on
  // the backend — the backend just emits "beep=true" hints and trusts us to
  // collapse bursts). Loaded once at mount and refreshed whenever the
  // SettingsModal saves.
  const settingsRef = useRef<MonitorSettings | null>(null);
  const lastBeepAtRef = useRef<number>(0);
  const loadSettings = async () => {
    try { settingsRef.current = await invoke<MonitorSettings>("monitor_get_settings"); }
    catch (e) { console.error("monitor_get_settings:", e); }
  };
  useEffect(() => { loadSettings(); }, []);

  // Single chokepoint for triggering a beep — every outage event funnels
  // through here. The cooldown drops anything that arrives within
  // `beep_cooldown_secs` of the previous beep, so a 12-node fleet flapping
  // at once plays one alert rather than a 12-tone chord.
  const tryBeep = () => {
    const s = settingsRef.current;
    if (!s || !s.beep_on_outage) return;
    const now = Date.now();
    const cooldownMs = Math.max(0, s.beep_cooldown_secs) * 1000;
    if (now - lastBeepAtRef.current < cooldownMs) return;
    lastBeepAtRef.current = now;
    beepNow();
  };

  // Tick once per second so "offline for Xs" badges advance even without
  // a new event firing.
  const [, forceTick] = useState(0);
  useEffect(() => {
    const id = window.setInterval(() => forceTick((n) => n + 1), 1000);
    return () => window.clearInterval(id);
  }, []);

  // ---- data load + live listeners --------------------------------------------

  const reload = async () => {
    try {
      const data = await invoke<MonitorRow[]>("monitor_list");
      setRows(data);
    } catch (e: any) {
      setError(String(e));
    }
  };
  useEffect(() => { reload(); /* eslint-disable-next-line */ }, []);

  useEffect(() => {
    // Async cleanup guard: listen() returns its unlisten via a Promise. If
    // the dep key changes (rows shape shifts) while some listen() promises
    // are still pending, the effect's cleanup runs against a still-partial
    // unlistens array and the late-arriving unlisten fns end up in an
    // abandoned array — silently leaking Tauri event handlers, so every
    // sample fires setLatest/setHistory/reload twice, four times, N times
    // over the app's lifetime. `cancelled` catches the race: if the effect
    // was torn down before a listen() promise resolved, we unlist it right
    // away instead of pushing it into a dead array.
    let cancelled = false;
    const unlistens: Array<() => void> = [];
    const track = (p: Promise<() => void>) => {
      p.then((fn) => {
        if (cancelled) fn();
        else unlistens.push(fn);
      });
    };
    rows.forEach((r) => {
      track(listen<MonitorRow>(`monitor-status-${r.node_id}`, () => reload()));
      track(listen<Sample>(`monitor-sample-${r.node_id}`, (event) => {
        const s = event.payload;
        if (!s) return;
        setLatest((prev) => ({ ...prev, [s.node_id]: s }));
        setHistory((prev) => {
          const nodeHist = { ...(prev[s.node_id] || {}) };
          for (const [k, v] of Object.entries(s.values)) {
            const arr = nodeHist[k] ? [...nodeHist[k], v] : [v];
            if (arr.length > HISTORY) arr.splice(0, arr.length - HISTORY);
            nodeHist[k] = arr;
          }
          return { ...prev, [s.node_id]: nodeHist };
        });
        // Keep `row.last_sample_ts` in sync so the "Xs ago" indicator
        // ticks immediately on each new sample instead of waiting for the
        // next status event / reload(). Without this the timer would
        // appear frozen between status changes.
        setRows((prev) => prev.map((r) =>
          r.node_id === s.node_id
            ? { ...r, last_sample_ts: s.ts, connected: true }
            : r
        ));
      }));
      // Outage + recovery events: always log; toast only if notify=true;
      // beep is gated by the local cooldown so multiple simultaneous
      // outages collapse into a single audible alert.
      track(listen<OutageEvent>(`monitor-outage-${r.node_id}`, (event) => {
        const o = event.payload;
        if (!o) return;
        const msg = `[Monitor] ${o.node_name} is OFFLINE — ${o.last_error}`;
        addLog?.(msg, "error");
        if (o.notify) showToast(`${o.node_name} offline · ${o.last_error}`, "err");
        if (o.beep) tryBeep();
        // Status reload so the card re-renders with the outage badge.
        reload();
      }));
      track(listen<RecoveredEvent>(`monitor-recovered-${r.node_id}`, (event) => {
        const o = event.payload;
        if (!o) return;
        const msg = `[Monitor] ${o.node_name} recovered after ${formatDuration(o.duration_ms)}`;
        addLog?.(msg, "success");
        if (o.notify) showToast(`${o.node_name} back online · was down ${formatDuration(o.duration_ms)}`, "ok");
        reload();
      }));
    });
    return () => {
      cancelled = true;
      unlistens.forEach((u) => u());
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [rows.map((r) => r.node_id).join(",")]);

  // ---- mutations --------------------------------------------------------------

  const wrap = async (fn: () => Promise<any>) => {
    setBusy(true); setError(null);
    try { await fn(); await reload(); }
    catch (e: any) { setError(String(e)); }
    finally { setBusy(false); }
  };

  const addNode = (nodeId: number) =>
    wrap(async () => { await invoke("monitor_add", { nodeId }); setAddOpen(false); });
  const removeNode = (nodeId: number) =>
    wrap(async () => {
      await invoke("monitor_remove", { nodeId });
      if (configFor === nodeId) setConfigFor(null);
      setHistory((p) => { const { [nodeId]: _, ...rest } = p; return rest; });
      setLatest((p) => { const { [nodeId]: _, ...rest } = p; return rest; });
    });
  const pauseNode = (nodeId: number) => wrap(() => invoke("monitor_pause", { nodeId }));
  const resumeNode = (nodeId: number) => wrap(() => invoke("monitor_resume", { nodeId }));
  const pauseAll = () => wrap(() => invoke("monitor_pause_all"));
  const resumeAll = () => wrap(() => invoke("monitor_resume_all"));
  const setMetrics = (nodeId: number, metrics: string[]) =>
    wrap(() => invoke("monitor_set_metrics", { nodeId, metrics }));
  const setCustoms = (nodeId: number, customs: CustomMetric[]) =>
    wrap(() => invoke("monitor_set_custom_metrics", { nodeId, customs }));

  // ---- derived ----------------------------------------------------------------

  const availableToAdd = useMemo(
    () => servers.filter((s: any) => !rows.some((r) => r.node_id === s.id)),
    [servers, rows]
  );
  const anyRunning = rows.some((r) => !r.paused);
  const allPaused = rows.length > 0 && rows.every((r) => r.paused);
  const configRow = configFor !== null ? rows.find((r) => r.node_id === configFor) || null : null;

  // ---- render -----------------------------------------------------------------

  return (
    <div className="flex flex-col h-full bg-[#09090b] relative">
      {/* Header — labels collapse to icon-only on narrow viewports so the
          control row never wraps into the title. The subtitle hides at the
          same breakpoint since it's the lowest-value piece of chrome. */}
      <div className="shrink-0 px-4 sm:px-6 py-3 sm:py-5 border-b border-white/5 flex items-center justify-between gap-2 sm:gap-3">
        <div className="min-w-0">
          <h2 className="text-[16px] sm:text-[22px] font-bold text-white uppercase tracking-tight flex items-center gap-2">
            <Activity size={18} className="text-primary" /> Monitoring
          </h2>
          <p className="hidden md:block text-[12px] text-zinc-300 mt-0.5">
            Per-node SSH polling. Outages are logged and (optionally) surfaced as toasts.
          </p>
        </div>
        <div className="flex items-center gap-1.5 sm:gap-2">
          <button
            onClick={() => setAddOpen(true)}
            title="Add node"
            className="h-9 px-2.5 sm:px-3.5 rounded-lg text-[12px] font-bold uppercase tracking-wider bg-primary/15 text-primary border border-primary/30 hover:bg-primary/25 flex items-center gap-1.5"
          >
            <Plus size={14} /> <span className="hidden sm:inline">Add Node</span>
          </button>
          <div className="hidden sm:block h-7 w-px bg-white/10" />
          {anyRunning ? (
            <button
              onClick={pauseAll}
              disabled={busy}
              title="Pause all"
              className="h-9 px-2.5 sm:px-3.5 rounded-lg text-[12px] font-bold uppercase tracking-wider bg-amber-500/15 text-amber-200 border border-amber-500/40 hover:bg-amber-500/25 disabled:opacity-50 flex items-center gap-1.5"
            >
              <Pause size={14} /> <span className="hidden sm:inline">Pause All</span>
            </button>
          ) : (
            <button
              onClick={resumeAll}
              disabled={busy || rows.length === 0}
              title={allPaused ? "Resume all" : "Start all"}
              className="h-9 px-2.5 sm:px-3.5 rounded-lg text-[12px] font-bold uppercase tracking-wider bg-emerald-500/15 text-emerald-200 border border-emerald-500/40 hover:bg-emerald-500/25 disabled:opacity-50 flex items-center gap-1.5"
            >
              <Play size={14} /> <span className="hidden sm:inline">{allPaused ? "Resume All" : "Start All"}</span>
            </button>
          )}
          <button
            onClick={() => setSettingsOpen(true)}
            className="h-9 w-9 rounded-lg text-zinc-300 hover:text-primary hover:bg-white/5 flex items-center justify-center"
            title="Monitor settings"
          >
            <SettingsIcon size={15} />
          </button>
          <button onClick={reload} className="h-9 w-9 rounded-lg text-zinc-300 hover:text-primary hover:bg-white/5 flex items-center justify-center" title="Refresh">
            <RefreshCw size={14} />
          </button>
        </div>
      </div>

      {error && (
        <div className="shrink-0 mx-6 mt-3 px-3 py-2 bg-rose-500/15 border border-rose-500/30 rounded text-rose-200 text-[12px] font-mono flex items-center gap-2">
          <AlertTriangle size={13} /> {error}
        </div>
      )}

      {/* Card grid — `min(100%, 290px)` keeps the layout from overflowing
          on viewports narrower than the card minimum; it collapses to a
          single full-width column on phones instead of clipping. */}
      <div className="flex-1 overflow-y-auto custom-scrollbar p-3 sm:p-6">
        {rows.length === 0 ? (
          <div className="h-full flex flex-col items-center justify-center text-zinc-300 text-[12px] font-mono gap-1 text-center px-4">
            <span>No monitored nodes yet.</span>
            <span className="text-zinc-500 text-[11px]">Click "+ Add Node" to pick from your saved nodes.</span>
          </div>
        ) : (
          <div className="grid gap-2 sm:gap-2.5" style={{ gridTemplateColumns: "repeat(auto-fill, minmax(min(100%, 290px), 1fr))" }}>
            {rows.map((r) => (
              <ServerCard
                key={r.node_id}
                row={r}
                sample={latest[r.node_id]}
                history={history[r.node_id] || {}}
                busy={busy}
                onPause={() => pauseNode(r.node_id)}
                onResume={() => resumeNode(r.node_id)}
                onConfig={() => setConfigFor(r.node_id)}
                onRemove={async () => {
                  if (!(await confirm({ title: "Stop monitoring this node?", message: "Custom metrics on this node will be lost.", destructive: true }))) return;
                  removeNode(r.node_id);
                }}
              />
            ))}
          </div>
        )}
      </div>

      {/* Floating toast */}
      {toast && (
        <div className="fixed bottom-5 right-6 z-[60] pointer-events-none">
          <div className={`px-3.5 py-2.5 rounded-lg shadow-2xl border font-mono text-[12px] flex items-start gap-2 max-w-[420px] ${
            toast.tone === "err" ? "bg-rose-950/90 border-rose-500/50 text-rose-100" :
            toast.tone === "ok"  ? "bg-emerald-950/90 border-emerald-500/50 text-emerald-100" :
                                   "bg-amber-950/90 border-amber-500/50 text-amber-100"
          }`}>
            {toast.tone === "err" ? <WifiOff size={14} className="mt-0.5 shrink-0" /> :
             toast.tone === "ok"  ? <Wifi size={14} className="mt-0.5 shrink-0" /> :
                                    <Bell size={14} className="mt-0.5 shrink-0" />}
            <span>{toast.msg}</span>
          </div>
        </div>
      )}

      {/* Configure slide-over */}
      {configRow && (
        <ConfigureSlideover
          row={configRow}
          busy={busy}
          onClose={() => setConfigFor(null)}
          onSetMetrics={(m) => setMetrics(configRow.node_id, m)}
          onSetCustoms={(c) => setCustoms(configRow.node_id, c)}
        />
      )}

      {/* Add modal */}
      {addOpen && (
        <div className="fixed inset-0 z-50 bg-black/60 backdrop-blur-sm flex items-center justify-center p-3" onClick={() => setAddOpen(false)}>
          <div className="w-full max-w-md max-h-[90vh] flex flex-col bg-[#121214] border border-white/10 rounded-xl shadow-2xl" onClick={(e) => e.stopPropagation()}>
            <div className="px-4 py-3 border-b border-white/5 flex items-center justify-between">
              <span className="text-[12px] font-bold uppercase tracking-widest text-white">Add Node to Monitor</span>
              <button onClick={() => setAddOpen(false)} className="text-zinc-400 hover:text-white"><X size={16} /></button>
            </div>
            <div className="p-2 flex-1 overflow-y-auto custom-scrollbar">
              {availableToAdd.length === 0 ? (
                <div className="text-center text-zinc-300 text-[12px] font-mono py-6">
                  All saved nodes are already being monitored.
                </div>
              ) : (
                availableToAdd.map((s: any) => (
                  <button
                    key={s.id}
                    onClick={() => addNode(s.id)}
                    disabled={busy}
                    className="w-full text-left px-3 py-2 rounded hover:bg-white/[0.05] disabled:opacity-50 flex items-center justify-between"
                  >
                    <div className="min-w-0">
                      <div className="text-[12.5px] font-bold text-zinc-50 truncate">{s.name}</div>
                      <div className="text-[11px] font-mono text-zinc-400 truncate">{s.host}:{s.port}</div>
                    </div>
                    <Plus size={14} className="text-primary shrink-0" />
                  </button>
                ))
              )}
            </div>
            <div className="px-4 py-3 border-t border-white/5">
              <p className="text-[11px] text-zinc-400 leading-snug">
                Node must already be approved (known-hosts). Added entries start <span className="text-zinc-200">paused</span> — click <span className="text-emerald-300">Resume</span> to begin polling.
              </p>
            </div>
          </div>
        </div>
      )}

      {/* Global settings modal */}
      {settingsOpen && (
        <SettingsModal
          onClose={() => setSettingsOpen(false)}
          // Refresh our local cached copy of settings so the new beep
          // cooldown takes effect on the very next outage — without this
          // the user would have to close and reopen Monitoring for the
          // change to apply.
          onSaved={(s) => { settingsRef.current = s; }}
        />
      )}
    </div>
  );
};

// ---------- ServerCard --------------------------------------------------------

interface ServerCardProps {
  row: MonitorRow;
  sample?: Sample;
  history: Record<string, number[]>;
  busy: boolean;
  onPause: () => void;
  onResume: () => void;
  onConfig: () => void;
  onRemove: () => void;
}

const ServerCard = ({ row, sample, history, busy, onPause, onResume, onConfig, onRemove }: ServerCardProps) => {
  // The outage badge is what makes a disconnection actionable — it stays
  // visible while the streak is active and shows the running duration.
  const inOutage = !!row.outage_since_ms && !row.paused;
  const stateChip =
    row.paused      ? { tone: "bg-zinc-500/15 border-zinc-500/40 text-zinc-300", Icon: Pause, label: "paused" }
    : inOutage      ? { tone: "bg-rose-500/20 border-rose-500/50 text-rose-200", Icon: WifiOff, label: "outage" }
    : !row.connected ? { tone: "bg-amber-500/15 border-amber-500/40 text-amber-200", Icon: WifiOff, label: "offline" }
                     : { tone: "bg-emerald-500/15 border-emerald-500/40 text-emerald-200", Icon: Wifi, label: "live" };
  const Chip = stateChip.Icon;
  const cardBorder = inOutage ? "border-rose-500/40 shadow-[0_0_0_1px_rgba(244,63,94,0.15)_inset]" : "border-white/5";

  const builtinCells = TOGGLE_KEYS
    .filter((tk) => row.enabled_metrics.includes(tk.key))
    .flatMap((tk) => tk.sub_keys.map((sk) => ({ kind: "builtin" as const, key: sk })));
  const customCells = row.custom_metrics.map((cm) => ({ kind: "custom" as const, cm }));

  return (
    <div className={`bg-[#0f0f12] border ${cardBorder} rounded-lg p-2.5 flex flex-col gap-1.5`}>
      {/* Header — name + chip + actions on one row, host/age below */}
      <div className="flex items-center justify-between gap-2">
        <div className="min-w-0 flex-1">
          <div className="flex items-center gap-1.5 min-w-0">
            <span className="text-[13px] font-bold text-zinc-50 truncate">{row.name}</span>
            <span className={`shrink-0 px-1.5 h-[16px] rounded border text-[9px] font-mono uppercase tracking-wider flex items-center gap-1 ${stateChip.tone}`}>
              <Chip size={9} /> {stateChip.label}
            </span>
          </div>
          <div className="text-[10px] text-zinc-400 font-mono truncate leading-tight">
            {row.host}:{row.port} <span className="text-zinc-500">· {timeAgo(row.last_sample_ts || undefined)}</span>
          </div>
        </div>
        <div className="flex items-center shrink-0">
          {row.paused ? (
            <button onClick={onResume} disabled={busy} title="Resume"
              className="w-6 h-6 rounded text-emerald-200 hover:bg-emerald-500/20 disabled:opacity-50 flex items-center justify-center">
              <Play size={11} />
            </button>
          ) : (
            <button onClick={onPause} disabled={busy} title="Pause"
              className="w-6 h-6 rounded text-amber-200 hover:bg-amber-500/20 disabled:opacity-50 flex items-center justify-center">
              <Pause size={11} />
            </button>
          )}
          <button onClick={onConfig} title="Configure"
            className="w-6 h-6 rounded text-zinc-300 hover:bg-white/5 hover:text-primary flex items-center justify-center">
            <Settings2 size={11} />
          </button>
          <button onClick={onRemove} disabled={busy} title="Remove"
            className="w-6 h-6 rounded text-zinc-300 hover:bg-rose-500/20 hover:text-rose-200 disabled:opacity-50 flex items-center justify-center">
            <Trash2 size={11} />
          </button>
        </div>
      </div>

      {/* Outage strip (sticky while down) */}
      {inOutage && (
        <div className="px-2 py-1 bg-rose-500/15 border border-rose-500/40 rounded text-[10.5px] text-rose-100 font-mono flex items-center gap-1.5">
          <WifiOff size={11} className="shrink-0" />
          <span className="font-bold">Offline {formatDuration(Date.now() - (row.outage_since_ms || Date.now()))}</span>
          <span className="text-rose-200/80 truncate flex-1" title={row.last_error || ""}>· {row.last_error || "no error reported"}</span>
        </div>
      )}

      {/* Non-outage transient error (single failed poll before threshold) */}
      {!inOutage && row.last_error && !row.paused && (
        <div className="px-1.5 py-0.5 bg-amber-500/10 border border-amber-500/30 rounded text-[10px] text-amber-200 font-mono truncate" title={row.last_error}>
          ⚠ {row.last_error}
        </div>
      )}

      {/* Metric rows — one per metric, full-width, label/value/sparkline inline */}
      {builtinCells.length + customCells.length === 0 ? (
        <div className="text-[11px] text-zinc-300 font-mono text-center py-3">
          No metrics. Click <Settings2 size={10} className="inline mb-0.5" /> to enable.
        </div>
      ) : (
        <div className="flex flex-col gap-[3px]">
          {builtinCells.map(({ key }) => {
            const meta = BUILTINS[key];
            const value = sample?.values?.[key];
            const hist = history[key] || [];
            return <CompactBuiltinRow key={key} metricKey={key} meta={meta} value={value} hist={hist} />;
          })}
          {customCells.map(({ cm }) => {
            const value = sample?.values?.[cm.id];
            const text = sample?.texts?.[cm.id];
            const err = sample?.errors?.[cm.id];
            const hist = history[cm.id] || [];
            return <CompactCustomRow key={cm.id} cm={cm} value={value} text={text} err={err} hist={hist} />;
          })}
        </div>
      )}
    </div>
  );
};

/// One-row built-in metric: [LABEL ……… VALUE | sparkline ………………]
/// Sparkline is responsive (fills remaining flex space) so a wider card
/// gets a wider sparkline without changing the rest of the layout.
const CompactBuiltinRow = ({ metricKey, meta, value, hist }: {
  metricKey: string; meta?: any; value?: number; hist: number[];
}) => (
  <div className="flex items-center gap-2 px-1.5 h-[22px] bg-white/[0.025] rounded hover:bg-white/[0.04] transition-colors">
    <span className="text-[10px] font-bold uppercase tracking-wider text-zinc-300 w-[58px] shrink-0 truncate">
      {meta?.label || metricKey}
    </span>
    <span className="text-[11px] font-mono font-bold text-zinc-50 w-[82px] shrink-0 text-right tabular-nums">
      {formatBuiltin(metricKey, value)}
    </span>
    <div className="flex-1 min-w-0">
      <Sparkline data={hist} width={140} height={16} color={meta?.color}
        min={meta?.is_pct ? 0 : undefined} max={meta?.is_pct ? 100 : undefined} />
    </div>
  </div>
);

const CompactCustomRow = ({ cm, value, text, err, hist }: {
  cm: CustomMetric; value?: number; text?: string; err?: string; hist: number[];
}) => (
  <div className="flex items-center gap-2 px-1.5 h-[22px] bg-amber-500/[0.06] border border-amber-500/20 rounded">
    <span className="text-[10px] font-bold uppercase tracking-wider text-amber-200 w-[58px] shrink-0 truncate flex items-center gap-1" title={cm.name}>
      {cm.parse === "text" && <FileCode2 size={9} className="text-amber-300 shrink-0" />}
      <span className="truncate">{cm.name}</span>
    </span>
    {err ? (
      <span className="flex-1 text-[10px] font-mono text-rose-200 truncate" title={err}>⚠ {err}</span>
    ) : cm.parse === "text" ? (
      <span className="flex-1 text-[10.5px] font-mono text-zinc-100 truncate" title={text}>
        {text || "—"}
      </span>
    ) : (
      <>
        <span className="text-[11px] font-mono font-bold text-zinc-50 w-[82px] shrink-0 text-right tabular-nums">
          {formatCustomNumber(value, cm.unit)}
        </span>
        <div className="flex-1 min-w-0">
          {cm.display === "sparkline" ? (
            <Sparkline data={hist} width={140} height={16} color={CUSTOM_COLOR} />
          ) : (
            <div className="h-[16px]" />
          )}
        </div>
      </>
    )}
  </div>
);

// ---------- SettingsModal ----------------------------------------------------

const SettingsModal = ({ onClose, onSaved }: {
  onClose: () => void;
  onSaved?: (s: MonitorSettings) => void;
}) => {
  const [s, setS] = useState<MonitorSettings | null>(null);
  const [saving, setSaving] = useState(false);
  const [err, setErr] = useState<string | null>(null);

  useEffect(() => {
    invoke<MonitorSettings>("monitor_get_settings")
      .then(setS)
      .catch((e) => setErr(String(e)));
  }, []);

  const save = async () => {
    if (!s) return;
    setSaving(true); setErr(null);
    try {
      const saved = await invoke<MonitorSettings>("monitor_set_settings", { newSettings: s });
      setS(saved);
      onSaved?.(saved);
      onClose();
    } catch (e: any) {
      setErr(String(e));
    } finally {
      setSaving(false);
    }
  };

  return (
    <div className="fixed inset-0 z-[60] bg-black/60 backdrop-blur-sm flex items-center justify-center p-3" onClick={onClose}>
      <div className="w-full max-w-md max-h-[90vh] flex flex-col bg-[#121214] border border-white/10 rounded-xl shadow-2xl" onClick={(e) => e.stopPropagation()}>
        <div className="px-4 py-3 border-b border-white/5 flex items-center justify-between">
          <span className="text-[12px] font-bold uppercase tracking-widest text-white flex items-center gap-2">
            <SettingsIcon size={13} className="text-primary" /> Monitor Settings
          </span>
          <button onClick={onClose} className="text-zinc-400 hover:text-white"><X size={16} /></button>
        </div>
        <div className="flex-1 overflow-y-auto custom-scrollbar p-5 space-y-4">
          {!s ? (
            <div className="text-center text-zinc-300 text-[12px] py-4 font-mono">Loading…</div>
          ) : (
            <>
              <NumberRow
                label="Poll Interval"
                hint="Seconds between samples (per node). Changes apply on next cycle."
                value={s.interval_secs} min={1} max={600} step={1} suffix="s"
                onChange={(v) => setS({ ...s, interval_secs: v })}
              />
              <NumberRow
                label="Connect Timeout"
                hint="How long to wait for TCP + SSH handshake before giving up."
                value={s.connect_timeout_secs} min={2} max={120} step={1} suffix="s"
                onChange={(v) => setS({ ...s, connect_timeout_secs: v })}
              />
              <NumberRow
                label="Poll Timeout"
                hint="How long to wait for the probe script's stdout each cycle."
                value={s.poll_timeout_secs} min={2} max={120} step={1} suffix="s"
                onChange={(v) => setS({ ...s, poll_timeout_secs: v })}
              />
              <NumberRow
                label="Outage Threshold"
                hint="Consecutive failures before declaring an outage and logging it."
                value={s.outage_threshold} min={1} max={100} step={1} suffix="× polls"
                onChange={(v) => setS({ ...s, outage_threshold: v })}
              />
              <label className="flex items-center justify-between gap-3 px-3 py-2 rounded bg-white/[0.03] border border-white/10 cursor-pointer">
                <div className="flex items-center gap-2">
                  {s.notify_on_outage ? <Bell size={14} className="text-amber-300" /> : <BellOff size={14} className="text-zinc-400" />}
                  <div>
                    <div className="text-[12px] font-bold text-zinc-100">Toast on Outage</div>
                    <div className="text-[10.5px] text-zinc-400">Show a popup when a node goes offline or recovers (always logs regardless).</div>
                  </div>
                </div>
                <input
                  type="checkbox"
                  className="w-4 h-4 accent-primary cursor-pointer"
                  checked={s.notify_on_outage}
                  onChange={(e) => setS({ ...s, notify_on_outage: e.target.checked })}
                />
              </label>

              {/* Beep toggle + a "test" button so the user can confirm
                  audio works before relying on it. */}
              <label className="flex items-center justify-between gap-3 px-3 py-2 rounded bg-white/[0.03] border border-white/10 cursor-pointer">
                <div className="flex items-center gap-2">
                  <Volume2 size={14} className={s.beep_on_outage ? "text-amber-300" : "text-zinc-400"} />
                  <div>
                    <div className="text-[12px] font-bold text-zinc-100">Beep on Outage</div>
                    <div className="text-[10.5px] text-zinc-400">Play a short tone when a node goes offline. Cross-platform synth — no system sound file required.</div>
                  </div>
                </div>
                <div className="flex items-center gap-2 shrink-0">
                  <button
                    type="button"
                    onClick={(e) => { e.preventDefault(); beepNow(); }}
                    title="Play a test beep"
                    className="h-7 px-2 rounded text-[10px] font-bold uppercase tracking-wider bg-white/5 text-zinc-200 border border-white/10 hover:bg-white/10"
                  >
                    Test
                  </button>
                  <input
                    type="checkbox"
                    className="w-4 h-4 accent-primary cursor-pointer"
                    checked={s.beep_on_outage}
                    onChange={(e) => setS({ ...s, beep_on_outage: e.target.checked })}
                  />
                </div>
              </label>
              <NumberRow
                label="Beep Cooldown"
                hint="Minimum seconds between consecutive beeps — collapses a burst of outages into a single alert."
                value={s.beep_cooldown_secs} min={0} max={600} step={1} suffix="s"
                onChange={(v) => setS({ ...s, beep_cooldown_secs: v })}
              />
              {err && (
                <div className="text-[11px] font-mono text-rose-200 px-2 py-1.5 bg-rose-500/15 border border-rose-500/30 rounded">{err}</div>
              )}
              <div className="flex items-center gap-2 pt-1">
                <button
                  onClick={save}
                  disabled={saving}
                  className="flex-1 h-9 rounded text-[11px] font-bold uppercase tracking-wider bg-primary/25 text-primary border border-primary/50 hover:bg-primary/35 disabled:opacity-50"
                >
                  {saving ? "Saving…" : "Apply"}
                </button>
                <button onClick={onClose} className="h-9 px-3 rounded text-[11px] font-bold uppercase tracking-wider bg-white/5 text-zinc-200 border border-white/10 hover:bg-white/10">
                  Cancel
                </button>
              </div>
            </>
          )}
        </div>
      </div>
    </div>
  );
};

const NumberRow = ({ label, hint, value, min, max, step, suffix, onChange }: {
  label: string; hint: string; value: number; min: number; max: number; step: number; suffix?: string;
  onChange: (v: number) => void;
}) => (
  <div className="px-3 py-2 rounded bg-white/[0.03] border border-white/10">
    <div className="flex items-center justify-between gap-3">
      <div className="min-w-0">
        <div className="text-[12px] font-bold text-zinc-100">{label}</div>
        <div className="text-[10.5px] text-zinc-400">{hint}</div>
      </div>
      <div className="flex items-center gap-1 shrink-0">
        <input
          type="number"
          min={min} max={max} step={step}
          value={value}
          onChange={(e) => {
            const v = parseInt(e.target.value);
            if (isNaN(v)) return;
            onChange(Math.max(min, Math.min(max, v)));
          }}
          className="w-20 h-8 px-2 bg-black/40 border border-white/10 rounded text-[12px] font-mono font-bold text-zinc-50 outline-none focus:border-primary/40 text-right"
        />
        {suffix && <span className="text-[10px] text-zinc-400 font-mono">{suffix}</span>}
      </div>
    </div>
  </div>
);

// ---------- ConfigureSlideover ----------------------------------------------

interface ConfigureProps {
  row: MonitorRow;
  busy: boolean;
  onClose: () => void;
  onSetMetrics: (m: string[]) => void;
  onSetCustoms: (c: CustomMetric[]) => void;
}

const ConfigureSlideover = ({ row, busy, onClose, onSetMetrics, onSetCustoms }: ConfigureProps) => {
  const [draft, setDraft] = useState<CustomMetric[]>(row.custom_metrics);
  useEffect(() => {
    setDraft(row.custom_metrics);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [row.node_id]);

  const toggleBuiltin = (key: string) => {
    const next = row.enabled_metrics.includes(key)
      ? row.enabled_metrics.filter((k) => k !== key)
      : [...row.enabled_metrics, key];
    onSetMetrics(next);
  };
  const updateDraft = (idx: number, patch: Partial<CustomMetric>) =>
    setDraft((prev) => prev.map((c, i) => (i === idx ? { ...c, ...patch } : c)));
  const removeDraft = (idx: number) =>
    setDraft((prev) => prev.filter((_, i) => i !== idx));
  const addDraft = () =>
    setDraft((prev) => [
      ...prev,
      { id: randomId(), name: "New metric", command: "echo 0", parse: "number", display: "sparkline" },
    ]);
  const dirty = JSON.stringify(draft) !== JSON.stringify(row.custom_metrics);

  return (
    <div className="fixed inset-0 z-50 bg-black/60 backdrop-blur-sm flex justify-end" onClick={onClose}>
      <div className="w-full max-w-[480px] bg-[#121214] border-l border-white/10 h-full flex flex-col" onClick={(e) => e.stopPropagation()}>
        <div className="shrink-0 px-4 py-3 border-b border-white/5 flex items-center justify-between">
          <div>
            <div className="text-[12px] font-bold uppercase tracking-widest text-white">Configure</div>
            <div className="text-[12px] text-zinc-300 font-mono mt-0.5">{row.name} <span className="text-zinc-500">· {row.host}:{row.port}</span></div>
          </div>
          <button onClick={onClose} className="text-zinc-400 hover:text-white"><X size={16} /></button>
        </div>

        <div className="flex-1 overflow-y-auto custom-scrollbar p-4 space-y-5">
          <section>
            <div className="text-[10px] font-bold uppercase tracking-widest text-zinc-300 mb-2">Built-in Metrics</div>
            <div className="flex flex-wrap gap-1.5">
              {TOGGLE_KEYS.map(({ key, label }) => {
                const on = row.enabled_metrics.includes(key);
                return (
                  <button
                    key={key}
                    onClick={() => toggleBuiltin(key)}
                    disabled={busy}
                    className={`h-7 px-2.5 rounded text-[10.5px] font-bold uppercase tracking-wider border transition-colors ${
                      on
                        ? "bg-primary/15 text-primary border-primary/40 hover:bg-primary/25"
                        : "bg-white/[0.03] text-zinc-300 border-white/10 hover:bg-white/[0.06] hover:text-zinc-100"
                    }`}
                  >
                    {label}
                  </button>
                );
              })}
            </div>
          </section>

          <section>
            <div className="flex items-center justify-between mb-2">
              <div className="text-[10px] font-bold uppercase tracking-widest text-zinc-300">Custom Commands</div>
              <button onClick={addDraft}
                className="h-6 px-2 rounded text-[10.5px] font-bold uppercase tracking-wider bg-amber-500/15 text-amber-200 border border-amber-500/40 hover:bg-amber-500/25 flex items-center gap-1">
                <Plus size={10} /> Add
              </button>
            </div>
            {draft.length === 0 ? (
              <div className="text-[11px] text-zinc-400 font-mono text-center py-4 border border-dashed border-white/10 rounded">
                No custom commands. Click <span className="text-amber-200">+ Add</span> to define one.
              </div>
            ) : (
              <div className="space-y-3">
                {draft.map((cm, idx) => (
                  <div key={cm.id} className="bg-white/[0.03] border border-white/10 rounded-lg p-3 space-y-2">
                    <div className="flex items-center gap-2">
                      <input
                        value={cm.name}
                        onChange={(e) => updateDraft(idx, { name: e.target.value })}
                        placeholder="Display name"
                        className="flex-1 h-7 px-2 bg-white/[0.04] border border-white/10 rounded text-[11.5px] text-zinc-50 outline-none focus:border-primary/40"
                      />
                      <button onClick={() => removeDraft(idx)}
                        className="w-7 h-7 rounded text-zinc-300 hover:text-rose-200 hover:bg-rose-500/15 flex items-center justify-center"
                        title="Remove">
                        <Trash2 size={12} />
                      </button>
                    </div>
                    <textarea
                      value={cm.command}
                      onChange={(e) => updateDraft(idx, { command: e.target.value })}
                      placeholder="shell command, e.g.  ps -e | wc -l"
                      rows={2}
                      className="w-full px-2 py-1.5 bg-black/40 border border-white/10 rounded text-[11.5px] font-mono text-zinc-100 outline-none focus:border-primary/40 resize-y"
                    />
                    <div className="grid grid-cols-3 gap-2 text-[10px]">
                      <label className="space-y-1">
                        <span className="text-zinc-400 uppercase tracking-wider font-bold">Parse</span>
                        <select value={cm.parse}
                          onChange={(e) => updateDraft(idx, { parse: e.target.value as any })}
                          className="w-full h-7 px-1.5 bg-white/[0.04] border border-white/10 rounded text-[11px] text-zinc-100 outline-none">
                          <option className="bg-[#1a1a1e]" value="number">First number</option>
                          <option className="bg-[#1a1a1e]" value="regex">Regex capture</option>
                          <option className="bg-[#1a1a1e]" value="text">Raw text</option>
                        </select>
                      </label>
                      <label className="space-y-1">
                        <span className="text-zinc-400 uppercase tracking-wider font-bold">Display</span>
                        <select value={cm.display}
                          onChange={(e) => updateDraft(idx, { display: e.target.value as any })}
                          disabled={cm.parse === "text"}
                          className="w-full h-7 px-1.5 bg-white/[0.04] border border-white/10 rounded text-[11px] text-zinc-100 outline-none disabled:opacity-50">
                          <option className="bg-[#1a1a1e]" value="sparkline">Sparkline</option>
                          <option className="bg-[#1a1a1e]" value="big_number">Big number</option>
                          <option className="bg-[#1a1a1e]" value="text">Text</option>
                        </select>
                      </label>
                      <label className="space-y-1">
                        <span className="text-zinc-400 uppercase tracking-wider font-bold">Unit</span>
                        <input value={cm.unit || ""}
                          onChange={(e) => updateDraft(idx, { unit: e.target.value })}
                          placeholder="%"
                          className="w-full h-7 px-2 bg-white/[0.04] border border-white/10 rounded text-[11px] text-zinc-100 outline-none focus:border-primary/40" />
                      </label>
                    </div>
                    {cm.parse === "regex" && (
                      <input value={cm.regex || ""}
                        onChange={(e) => updateDraft(idx, { regex: e.target.value })}
                        placeholder="regex with one capture group, e.g.   load average:\s*([0-9.]+)"
                        className="w-full h-7 px-2 bg-white/[0.04] border border-white/10 rounded text-[11px] font-mono text-zinc-100 outline-none focus:border-primary/40" />
                    )}
                  </div>
                ))}
              </div>
            )}
            {dirty && (
              <button onClick={() => onSetCustoms(draft)} disabled={busy}
                className="mt-3 w-full h-9 rounded text-[11px] font-bold uppercase tracking-wider bg-primary/25 text-primary border border-primary/50 hover:bg-primary/35 disabled:opacity-50">
                Apply Custom Commands
              </button>
            )}
          </section>

          <p className="text-[11px] text-zinc-400 leading-snug">
            Custom commands run on the remote shell every poll cycle alongside the built-in probe. <span className="text-amber-200">Regex</span> captures the first group as a number; <span className="text-amber-200">Raw text</span> shows the trimmed stdout as-is (no history).
          </p>
        </div>
      </div>
    </div>
  );
};

export default MonitoringPanel;
