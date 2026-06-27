import { useEffect, useMemo, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import {
  RefreshCw, AlertTriangle, Loader2, Server, Network as NetIcon,
  Cog, Cpu, MemoryStick, HardDrive, Clock, Activity, Wifi,
  AlertCircle, Search, ShieldAlert, Plug, Play, Square, RotateCw, Box,
  Shield,
} from "lucide-react";
import DockerTab from "./DockerTab";
import { ScrollableTabs } from "./ScrollableTabs";

interface InfoPanelProps {
  sessionId: string;
  disabled?: boolean;
  // Called when the user wants to open a `docker exec -it` terminal into a
  // specific container. SessionView spawns a new terminal tab with the
  // container exec command and switches focus to it. Optional because non-
  // SessionView consumers (e.g. tests, storybook) don't need to wire it.
  onOpenContainerTerminal?: (containerName: string, useSudo: boolean) => void;
}

interface SectionResult {
  data: string;
  truncated: boolean;
  exec_ms: number;
}

interface SystemctlResult {
  success: boolean;
  exit_code: number;
  stdout: string;
  stderr: string;
  used_sudo: boolean;
}

type SubTab = "overview" | "network" | "ports" | "services" | "docker";
const TABS: { id: SubTab; label: string; icon: any }[] = [
  { id: "overview", label: "Overview", icon: Server },
  { id: "network",  label: "Network",  icon: NetIcon },
  { id: "ports",    label: "Ports",    icon: Plug },
  { id: "services", label: "Services", icon: Cog },
  { id: "docker",   label: "Docker",   icon: Box },
];

// Visible caps. Servers with hundreds of veth/dummy interfaces (think a
// Docker host with 200 containers, each spawning a veth pair) would
// otherwise dump 400 cards into the DOM and tank scroll perf. Render the
// first slice and let the user expand if they really need them all.
const NIC_INITIAL_CAP = 20;

// ---------- formatters ----------

function formatBytes(n: number): string {
  if (!isFinite(n) || n <= 0) return "0 B";
  const units = ["B", "KB", "MB", "GB", "TB", "PB"];
  let i = 0;
  let v = n;
  while (v >= 1024 && i < units.length - 1) { v /= 1024; i++; }
  return `${v.toFixed(v >= 100 ? 0 : v >= 10 ? 1 : 2)} ${units[i]}`;
}

function parseUptime(line: string): string {
  if (!line) return "—";
  const m = line.match(/up\s+(.+?),\s+\d+\s+user/);
  if (m) return m[1].trim();
  const m2 = line.match(/up\s+(.+?),\s+load/);
  if (m2) return m2[1].trim();
  return line.trim();
}

// ---------- OVERVIEW ----------

function parseFreeBytes(out: string): { totalMem: number; usedMem: number; totalSwap: number; usedSwap: number } | null {
  const lines = out.split(/\r?\n/);
  let totalMem = 0, usedMem = 0, totalSwap = 0, usedSwap = 0;
  let ok = false;
  for (const l of lines) {
    const parts = l.trim().split(/\s+/);
    if (parts[0] === "Mem:" && parts.length >= 3) {
      totalMem = parseInt(parts[1], 10) || 0;
      usedMem = parseInt(parts[2], 10) || 0;
      if (parts.length >= 7) {
        const avail = parseInt(parts[6], 10) || 0;
        if (avail > 0) usedMem = totalMem - avail;
      }
      ok = true;
    } else if (parts[0] === "Swap:" && parts.length >= 3) {
      totalSwap = parseInt(parts[1], 10) || 0;
      usedSwap = parseInt(parts[2], 10) || 0;
    }
  }
  return ok ? { totalMem, usedMem, totalSwap, usedSwap } : null;
}

interface DiskRow { device: string; fsType: string; size: number; used: number; avail: number; mount: string; }
function parseDfPT(out: string): DiskRow[] {
  const lines = out.split(/\r?\n/).filter(Boolean);
  if (lines.length < 2) return [];
  const rows: DiskRow[] = [];
  for (let i = 1; i < lines.length; i++) {
    const parts = lines[i].trim().split(/\s+/);
    if (parts.length < 7) continue;
    const fsType = parts[1];
    if (["tmpfs", "devtmpfs", "squashfs", "overlay", "proc", "sysfs", "cgroup", "cgroup2", "ramfs", "autofs"].includes(fsType)) continue;
    rows.push({
      device: parts[0],
      fsType,
      size: (parseInt(parts[2], 10) || 0) * 1024,
      used: (parseInt(parts[3], 10) || 0) * 1024,
      avail: (parseInt(parts[4], 10) || 0) * 1024,
      mount: parts.slice(6).join(" "),
    });
  }
  return rows;
}

interface OverviewData {
  hostname: string; os: string; uname: string; uptime: string;
  mem: { totalMem: number; usedMem: number; totalSwap: number; usedSwap: number } | null;
  disks: DiskRow[]; cpuCount: number;
  load: { l1: number; l5: number; l15: number } | null;
}
function parseOverview(raw: string): OverviewData {
  // The script prints sep at the start too, so the first split chunk is empty.
  // We drop it and re-index from 1.
  const parts = raw.split("__SUB_INFO_OV_SEP__").map(s => s.trim());
  const get = (i: number) => parts[i] ?? "";
  const loadLine = get(8);
  let load: OverviewData["load"] = null;
  if (loadLine) {
    const p = loadLine.split(/\s+/);
    if (p.length >= 3) load = { l1: parseFloat(p[0]), l5: parseFloat(p[1]), l15: parseFloat(p[2]) };
  }
  return {
    hostname: get(1),
    os: get(2),
    uname: get(3),
    uptime: parseUptime(get(4)),
    mem: parseFreeBytes(get(5)),
    disks: parseDfPT(get(6)),
    cpuCount: parseInt(get(7), 10) || 0,
    load,
  };
}

// ---------- NETWORK ----------

interface NicAddr { family: string; local: string; prefixlen: number; }
interface Nic { name: string; state: string; mac: string; mtu: number; addrs: NicAddr[]; }
function parseNics(raw: string): Nic[] {
  if (!raw) return [];
  try {
    const arr = JSON.parse(raw);
    if (!Array.isArray(arr)) return [];
    const nics: Nic[] = arr.map((n: any) => ({
      name: n.ifname || "?",
      state: n.operstate || (Array.isArray(n.flags) && n.flags.includes("UP") ? "UP" : "DOWN"),
      mac: n.address || "",
      mtu: n.mtu || 0,
      addrs: (n.addr_info || []).map((a: any) => ({
        family: a.family || "",
        local: a.local || "",
        prefixlen: a.prefixlen || 0,
      })),
    }));
    // UP first (user explicitly asked for this sort), then by name. Loopback
    // is always present so we anchor it last among UPs so real NICs come
    // first — that's almost always what the user is looking for.
    return nics.sort((a, b) => {
      const aUp = a.state === "UP" ? 0 : 1;
      const bUp = b.state === "UP" ? 0 : 1;
      if (aUp !== bUp) return aUp - bUp;
      const aLo = a.name === "lo" ? 1 : 0;
      const bLo = b.name === "lo" ? 1 : 0;
      if (aLo !== bLo) return aLo - bLo;
      return a.name.localeCompare(b.name);
    });
  } catch { return []; }
}

interface RouteRow { dst: string; via: string; dev: string; }
function parseRoutes(raw: string): RouteRow[] {
  if (!raw) return [];
  try {
    const arr = JSON.parse(raw);
    if (!Array.isArray(arr)) return [];
    return arr.map((r: any): RouteRow => ({
      dst: r.dst || "default", via: r.gateway || "", dev: r.dev || "",
    }));
  } catch { return []; }
}

// Firewall section. Engine prefix tells us which tool produced the rules and
// whether sudo was needed; `available=false` means we couldn't read the
// table at all (no tool installed, or both direct + sudo -n failed).
type FwEngine = "iptables" | "nft" | "none";
interface FirewallData {
  engine: FwEngine;
  usedSudo: boolean;
  available: boolean;
  denied: boolean;        // engine present but ruleset unreadable (perm denied)
  raw: string;            // raw rules text, empty when denied/none
}
function parseFirewall(section: string): FirewallData {
  const trimmed = section.trim();
  if (!trimmed) return { engine: "none", usedSudo: false, available: false, denied: false, raw: "" };
  const nl = trimmed.indexOf("\n");
  const header = nl >= 0 ? trimmed.slice(0, nl).trim() : trimmed;
  const body = nl >= 0 ? trimmed.slice(nl + 1) : "";
  const m = header.match(/^FW:(.+)$/);
  if (!m) return { engine: "none", usedSudo: false, available: false, denied: false, raw: "" };
  const tag = m[1];
  if (tag === "none") return { engine: "none", usedSudo: false, available: false, denied: false, raw: "" };
  if (tag.endsWith("-denied")) {
    const eng = tag.startsWith("iptables") ? "iptables" : "nft";
    return { engine: eng, usedSudo: false, available: false, denied: true, raw: "" };
  }
  const usedSudo = tag.endsWith("-sudo");
  const eng: FwEngine = tag.startsWith("iptables") ? "iptables" : "nft";
  return { engine: eng, usedSudo, available: true, denied: false, raw: body };
}

interface NetworkData { nics: Nic[]; routes: RouteRow[]; firewall: FirewallData; }
function parseNetwork(raw: string): NetworkData {
  const parts = raw.split("__SUB_INFO_NET_SEP__").map(s => s.trim());
  return {
    nics: parseNics(parts[1] ?? ""),
    routes: parseRoutes(parts[2] ?? ""),
    firewall: parseFirewall(parts[3] ?? ""),
  };
}

// ---------- PORTS ----------

interface PortRow {
  proto: string;       // tcp | udp
  state: string;       // LISTEN | UNCONN | ...
  localAddr: string;
  localPort: string;
  process: string;     // best-effort program name
  pid: string;
  hidden: boolean;     // true if process column was empty due to permissions
}

// Detect the engine line printed by the probe so we can pick the right parser.
function detectPortsEngine(raw: string): { engine: "ss" | "netstat" | "unknown"; body: string } {
  const m = raw.match(/^ENGINE:(ss|netstat)\s*\n([\s\S]*)/);
  if (m) return { engine: m[1] as "ss" | "netstat", body: m[2] };
  return { engine: "unknown", body: raw };
}

function parsePortsSs(body: string): PortRow[] {
  // `ss -tulnpH` columns:
  // Netid State Recv-Q Send-Q LocalAddress:Port PeerAddress:Port Process
  // e.g. `tcp LISTEN 0 128 0.0.0.0:22 0.0.0.0:* users:(("sshd",pid=1234,fd=3))`
  const rows: PortRow[] = [];
  for (const line of body.split(/\r?\n/)) {
    const t = line.trim();
    if (!t) continue;
    const parts = t.split(/\s+/);
    if (parts.length < 5) continue;
    const proto = parts[0];
    if (!/^(tcp|udp)/i.test(proto)) continue;
    const state = parts[1] || "";
    const local = parts[4] || "";
    const procField = parts.slice(6).join(" ") || "";
    const userMatch = procField.match(/users:\(\(("([^"]+)"),pid=(\d+)/);
    const altMatch = userMatch || procField.match(/"([^"]+)",pid=(\d+)/);
    const proc = userMatch ? userMatch[2] : (altMatch ? altMatch[1] : "");
    const pid = userMatch ? userMatch[3] : (altMatch ? altMatch[2] : "");
    const sep = local.lastIndexOf(":");
    const addr = sep > 0 ? local.slice(0, sep) : local;
    const port = sep > 0 ? local.slice(sep + 1) : "";
    rows.push({
      proto: proto.toLowerCase(),
      state,
      localAddr: addr,
      localPort: port,
      process: proc,
      pid,
      hidden: !proc,
    });
  }
  return rows;
}

function parsePortsNetstat(body: string): PortRow[] {
  // `netstat -tulnp` columns: Proto Recv-Q Send-Q LocalAddress ForeignAddress State PID/Program
  const rows: PortRow[] = [];
  for (const line of body.split(/\r?\n/)) {
    const t = line.trim();
    if (!t || /^(Active|Proto)/i.test(t)) continue;
    const parts = t.split(/\s+/);
    if (parts.length < 6) continue;
    const proto = parts[0];
    if (!/^(tcp|udp)/i.test(proto)) continue;
    const local = parts[3] || "";
    const state = /^udp/i.test(proto) ? "UNCONN" : (parts[5] || "");
    const procIdx = /^udp/i.test(proto) ? 5 : 6;
    const procField = parts[procIdx] || "";
    let proc = "", pid = "";
    if (procField && procField !== "-") {
      const m = procField.match(/^(\d+)\/(.+)$/);
      if (m) { pid = m[1]; proc = m[2]; }
    }
    const sep = local.lastIndexOf(":");
    const addr = sep > 0 ? local.slice(0, sep) : local;
    const port = sep > 0 ? local.slice(sep + 1) : "";
    rows.push({
      proto: proto.toLowerCase().replace(/^tcp.*/, "tcp").replace(/^udp.*/, "udp"),
      state,
      localAddr: addr,
      localPort: port,
      process: proc,
      pid,
      hidden: !proc,
    });
  }
  return rows;
}

interface PortsData {
  engine: "ss" | "netstat" | "unknown";
  rows: PortRow[];
  partiallyHidden: boolean;
  available: boolean;
}
function parsePorts(raw: string): PortsData {
  const { engine, body } = detectPortsEngine(raw);
  if (engine === "unknown") return { engine, rows: [], partiallyHidden: false, available: false };
  const rows = engine === "ss" ? parsePortsSs(body) : parsePortsNetstat(body);
  const partiallyHidden = rows.some(r => r.hidden);
  return { engine, rows, partiallyHidden, available: true };
}

// ---------- SERVICES ----------

interface ServiceRow {
  unit: string;
  load: string;        // loaded | not-found | masked
  active: string;      // active | inactive | failed | activating
  sub: string;         // running | dead | exited | failed
  desc: string;
}
function parseServices(raw: string): { available: boolean; rows: ServiceRow[] } {
  const m = raw.match(/^ENGINE:(systemd|none)\s*\n([\s\S]*)/);
  if (!m || m[1] !== "systemd") return { available: false, rows: [] };
  const body = m[2];
  const rows: ServiceRow[] = [];
  for (const line of body.split(/\r?\n/)) {
    const t = line.trim();
    if (!t) continue;
    // `●` markers from systemctl status (we use --plain to drop them, but be safe).
    const cleaned = t.replace(/^[●○]\s*/, "");
    const parts = cleaned.split(/\s+/);
    if (parts.length < 4) continue;
    rows.push({
      unit: parts[0],
      load: parts[1],
      active: parts[2],
      sub: parts[3],
      desc: parts.slice(4).join(" "),
    });
  }
  return { available: true, rows };
}

// ---------- DOCKER ----------
// Docker tab now lives in DockerTab.tsx — it does its own lazy per-sub-tab
// fetching via dedicated docker commands instead of the bulky info-section
// probe, so first paint shows containers in ~200 ms rather than waiting for
// `docker stats` / `docker system df` to round-trip.

// Common error mapper — turn raw error strings into something a human can act on.
function describeError(err: string): { title: string; hint?: string } {
  const e = err.toLowerCase();
  if (e.includes("session is not connected") || e.includes("session not connected")) {
    return { title: "Session is disconnected", hint: "Reconnect to fetch fresh data." };
  }
  if (e.includes("timed out") || e.includes("timeout")) {
    return { title: "Server didn't respond in time", hint: "It may be under load. Try again, or run the command in the terminal." };
  }
  if (e.includes("permission denied") || e.includes("not authorized") || e.includes("interactive authentication")) {
    return { title: "Permission denied", hint: "This action needs root. Configure NOPASSWD sudo or run as root." };
  }
  return { title: err || "Probe failed" };
}

// ---------- TAB STATE ----------

interface TabState<T> {
  loaded: boolean;
  loading: boolean;
  error: string | null;
  data: T | null;
  meta: { exec_ms: number; truncated: boolean } | null;
}
function freshTab<T>(): TabState<T> {
  return { loaded: false, loading: false, error: null, data: null, meta: null };
}

const subTabBase = "shrink-0 h-9 sm:h-8 px-3 rounded-lg flex items-center gap-1.5 text-[11px] font-bold uppercase tracking-wider transition-all";
const subTabIdle = "text-zinc-400 bg-white/[0.04] border border-white/10 hover:bg-white/[0.08] hover:text-white";
const subTabActive = "bg-primary/10 text-primary border border-primary/20 shadow-inner";

const InfoPanel = ({ sessionId, disabled, onOpenContainerTerminal }: InfoPanelProps) => {
  const [tab, setTab] = useState<SubTab>("overview");

  const [overview, setOverview] = useState<TabState<OverviewData>>(freshTab);
  const [network, setNetwork] = useState<TabState<NetworkData>>(freshTab);
  const [ports, setPorts] = useState<TabState<PortsData>>(freshTab);
  const [services, setServices] = useState<TabState<{ available: boolean; rows: ServiceRow[] }>>(freshTab);
  // Docker tab has its own internal state (lives in DockerTab.tsx) so we
  // don't allocate a TabState here. The umbrella InfoPanel just renders
  // the component; its lazy fetch ladder runs independently.

  // Excludes docker — DockerTab owns its own per-sub-tab state and bypasses
  // this generic ssh_info_probe_section pipeline. Looking the docker tab up
  // here would be a type error AND a behavioural bug (running the heavy
  // umbrella probe needlessly).
  type ProbeSubTab = Exclude<SubTab, "docker">;
  const stateMap: Record<ProbeSubTab, readonly [TabState<any>, React.Dispatch<React.SetStateAction<TabState<any>>>]> = {
    overview: [overview, setOverview] as const,
    network:  [network, setNetwork] as const,
    ports:    [ports, setPorts] as const,
    services: [services, setServices] as const,
  };
  // Synthetic "loaded" state for the docker tab so the header meta line and
  // refresh button can render uniformly; the real status lives inside
  // DockerTab itself.
  const dockerSentinel: TabState<null> = { loaded: true, loading: false, error: null, data: null, meta: null };
  const activeState: TabState<any> = tab === "docker" ? dockerSentinel : stateMap[tab as ProbeSubTab][0];

  const fetchSection = async (which: SubTab) => {
    if (disabled) return;
    if (which === "docker") return; // handled inside DockerTab
    const setter = stateMap[which as ProbeSubTab][1] as (s: any) => void;
    setter((prev: TabState<any>) => ({ ...prev, loading: true, error: null }));
    try {
      const r = await invoke<SectionResult>("ssh_info_probe_section", { sessionId, section: which });
      let parsed: any;
      switch (which) {
        case "overview": parsed = parseOverview(r.data); break;
        case "network":  parsed = parseNetwork(r.data); break;
        case "ports":    parsed = parsePorts(r.data); break;
        case "services": parsed = parseServices(r.data); break;
      }
      setter({
        loaded: true, loading: false, error: null,
        data: parsed,
        meta: { exec_ms: r.exec_ms, truncated: r.truncated },
      });
    } catch (e: any) {
      const msg = typeof e === "string" ? e : (e?.message || "Probe failed");
      setter((prev: TabState<any>) => ({ ...prev, loading: false, error: msg }));
    }
  };

  // Lazy first fetch: when the user activates a tab that's never been
  // loaded, fire its probe exactly once. Per the user's spec, "every tab
  // should fetch when clicked, not all at once." The docker tab is the
  // exception — DockerTab does its own internal lazy fetching across its
  // sub-sub-tabs, so we skip the umbrella probe for it.
  useEffect(() => {
    if (disabled) return;
    if (tab === "docker") return;
    const [st] = stateMap[tab as ProbeSubTab];
    if (!st.loaded && !st.loading) {
      fetchSection(tab);
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [tab, disabled, sessionId]);

  // ---------- Tab strip ----------
  // Wrapped in ScrollableTabs so narrow widths (split panes, embedded phone
  // layouts, etc.) still let the user reach every tab — chevron buttons fade
  // in when overflow is detected, and the active tab auto-scrolls into view.
  const SubTabStrip = (
    <div className="shrink-0 border-b border-white/5 bg-white/[0.02] px-3 py-2">
      <ScrollableTabs>
        {TABS.map(t => {
          const Icon = t.icon;
          return (
            <button
              key={t.id}
              data-active={tab === t.id}
              onClick={() => setTab(t.id)}
              className={`${subTabBase} ${tab === t.id ? subTabActive : subTabIdle} snap-start`}
            >
              <Icon size={12} /> {t.label}
            </button>
          );
        })}
      </ScrollableTabs>
    </div>
  );

  const fmtMeta = activeState.meta
    ? `Probed in ${activeState.meta.exec_ms} ms${activeState.meta.truncated ? " · output truncated" : ""}`
    : activeState.loading ? "Probing…" : activeState.loaded ? "" : "Pending";

  return (
    <div className="flex-1 flex flex-col overflow-hidden">
      <div className="shrink-0 h-9 px-3 flex items-center justify-between border-b border-white/5 bg-white/[0.02] gap-2">
        <span className="text-[10px] text-zinc-500 truncate select-text">{fmtMeta}</span>
        <button
          onClick={() => fetchSection(tab)}
          disabled={activeState.loading || disabled}
          title="Refresh this tab"
          className="h-9 sm:h-7 px-3 sm:px-2.5 rounded-md flex items-center gap-1.5 text-[10.5px] font-bold uppercase tracking-wider text-zinc-300 bg-white/[0.04] border border-white/10 hover:bg-white/[0.08] hover:text-white disabled:opacity-40 disabled:cursor-not-allowed transition-all"
        >
          {activeState.loading ? <Loader2 size={12} className="animate-spin" /> : <RefreshCw size={12} />}
          <span>Refresh</span>
        </button>
      </div>

      {SubTabStrip}

      <div className="flex-1 overflow-y-auto custom-scrollbar p-3 space-y-3 select-text">
        {disabled && (
          <BannerIcon tone="amber" icon={<AlertTriangle size={14} />}>
            Session is not connected. Reconnect to fetch server info.
          </BannerIcon>
        )}

        {activeState.error && (() => {
          const e = describeError(activeState.error!);
          return (
            <div className="flex items-start gap-2 px-3 py-2 rounded-lg bg-red-500/5 border border-red-500/20 text-red-300 text-[11px]">
              <AlertCircle size={14} className="mt-0.5 shrink-0" />
              <div className="min-w-0">
                <div className="font-bold uppercase tracking-wider">{e.title}</div>
                {e.hint && <div className="text-red-200/80 mt-0.5">{e.hint}</div>}
                <div className="opacity-70 mt-0.5 font-mono break-all">{activeState.error}</div>
              </div>
            </div>
          );
        })()}

        {activeState.loading && !activeState.data && (
          <div className="flex items-center gap-2 text-zinc-400 text-[11px]">
            <Loader2 size={14} className="animate-spin" /> Probing {tab}…
          </div>
        )}

        {tab === "overview" && overview.data && <OverviewTab data={overview.data} />}
        {tab === "network"  && network.data  && <NetworkTab data={network.data} />}
        {tab === "ports"    && ports.data    && <PortsTab data={ports.data} />}
        {tab === "services" && services.data && (
          <ServicesTab
            sessionId={sessionId}
            data={services.data}
            onRefresh={() => fetchSection("services")}
          />
        )}
        {tab === "docker" && (
          <DockerTab
            sessionId={sessionId}
            disabled={disabled}
            onOpenContainerTerminal={(name, useSudo) => onOpenContainerTerminal?.(name, useSudo)}
          />
        )}
      </div>
    </div>
  );
};

// ============== Sub-tabs ==============

// Overview lays its cards strictly 2-per-row on any width ≥560px (1-per-row
// below) — the user explicitly asked for "pair, pair" instead of the previous
// 3-up grid that felt cramped. The bottom row pairs Memory with Disks; both
// use `items-stretch` so they share a track height (looks balanced even
// when Memory has fewer rows than Disks) and the Disks card's body scrolls
// internally past a few entries so it doesn't shove Memory off-screen on
// short windows.
const OverviewTab = ({ data }: { data: OverviewData }) => {
  const cpuTxt = data.cpuCount ? `${data.cpuCount} core${data.cpuCount === 1 ? "" : "s"}` : "—";
  return (
    <div className="min-h-full flex flex-col gap-4">
      {/* Row 1: Identity | Runtime */}
      <div className="grid gap-4 grid-cols-1 [@media(min-width:560px)]:grid-cols-2 items-stretch">
        <InfoCard title="Identity" icon={<Server size={12} />} className="h-full" bodyClassName="!p-4 space-y-1">
          <KV label="Hostname" value={data.hostname || "—"} />
          <KV label="OS" value={data.os || "—"} />
          <KV label="Kernel" value={data.uname || "—"} />
        </InfoCard>

        <InfoCard title="Runtime" icon={<Clock size={12} />} className="h-full" bodyClassName="!p-4 space-y-1">
          <KV label="Uptime" value={data.uptime} icon={<Clock size={11} />} />
          {data.load && (
            <KV label="Load avg" value={`${data.load.l1.toFixed(2)}  ${data.load.l5.toFixed(2)}  ${data.load.l15.toFixed(2)}`} icon={<Activity size={11} />} />
          )}
          <KV label="CPU" value={cpuTxt} icon={<Cpu size={11} />} />
        </InfoCard>
      </div>

      {/* Row 2: Memory | Disks (paired). Disks grows to take remaining
          vertical space via flex-1 on this wrapper, and its body scrolls
          internally so Memory isn't squashed when there are many disks. */}
      <div className="grid gap-4 grid-cols-1 [@media(min-width:560px)]:grid-cols-2 items-stretch flex-1 min-h-0">
        {data.mem ? (
          <InfoCard title="Memory" icon={<MemoryStick size={12} />} className="h-full" bodyClassName="!p-4 space-y-3">
            <Meter label="RAM" used={data.mem.usedMem} total={data.mem.totalMem} />
            {data.mem.totalSwap > 0 && (
              <Meter label="Swap" used={data.mem.usedSwap} total={data.mem.totalSwap} />
            )}
          </InfoCard>
        ) : <div />}

        {data.disks.length > 0 ? (
          <InfoCard
            title={`Disks · ${data.disks.length}`}
            icon={<HardDrive size={12} />}
            className="h-full flex flex-col min-h-0"
            bodyClassName="!p-4 flex-1 min-h-0 overflow-y-auto custom-scrollbar space-y-3"
          >
            {data.disks.map((d, i) => (
              <div key={i}>
                <div className="flex items-baseline justify-between text-[11px] mb-1 gap-2">
                  <span className="text-zinc-200 font-mono truncate min-w-0">{d.mount}</span>
                  <span className="text-zinc-500 shrink-0 text-[10px]">{d.fsType} · {d.device}</span>
                </div>
                <Meter label="" used={d.used} total={d.size} compact />
              </div>
            ))}
          </InfoCard>
        ) : <div />}
      </div>
    </div>
  );
};

const NetworkTab = ({ data }: { data: NetworkData }) => {
  const [showAllNics, setShowAllNics] = useState(false);
  const [filter, setFilter] = useState("");
  const filtered = useMemo(() => {
    if (!filter.trim()) return data.nics;
    const f = filter.toLowerCase();
    return data.nics.filter(n =>
      n.name.toLowerCase().includes(f) ||
      n.mac.toLowerCase().includes(f) ||
      n.addrs.some(a => a.local.toLowerCase().includes(f))
    );
  }, [data.nics, filter]);
  const visible = showAllNics ? filtered : filtered.slice(0, NIC_INITIAL_CAP);
  const upCount = data.nics.filter(n => n.state === "UP").length;

  return (
    <div className="space-y-3">
      <InfoCard
        title={`Interfaces · ${upCount} up / ${data.nics.length} total`}
        icon={<Wifi size={12} />}
        actions={
          <FilterInput value={filter} onChange={setFilter} placeholder="Filter by name / IP / MAC" />
        }
      >
        {filtered.length === 0 ? (
          <Empty>No NICs match.</Empty>
        ) : (
          <div className="grid grid-cols-1 [@media(min-width:560px)]:grid-cols-2 gap-2">
            {visible.map(n => (
              <div key={n.name} className="bg-black/30 border border-white/5 rounded-lg p-2.5">
                <div className="flex items-center justify-between gap-2 mb-1.5">
                  <span className="text-[12px] font-mono font-bold text-white truncate">{n.name}</span>
                  <StatusBadge tone={n.state === "UP" ? "green" : "zinc"}>{n.state}</StatusBadge>
                </div>
                <div className="text-[10px] text-zinc-500 font-mono mb-1 truncate">
                  {n.mac || "—"} · MTU {n.mtu || "—"}
                </div>
                {n.addrs.length === 0 ? (
                  <div className="text-[10px] text-zinc-600 italic">no addresses</div>
                ) : (
                  <div className="space-y-0.5">
                    {n.addrs.map((a, i) => (
                      <div key={i} className="text-[11px] font-mono text-zinc-300 flex items-center gap-2">
                        <span className="text-[9px] uppercase text-zinc-500 w-6 shrink-0">
                          {a.family === "inet" ? "v4" : a.family === "inet6" ? "v6" : a.family}
                        </span>
                        <span className="truncate">{a.local}<span className="text-zinc-500">/{a.prefixlen}</span></span>
                      </div>
                    ))}
                  </div>
                )}
              </div>
            ))}
          </div>
        )}
        {filtered.length > NIC_INITIAL_CAP && (
          <button
            onClick={() => setShowAllNics(s => !s)}
            className="mt-2 w-full h-8 rounded-lg border border-white/10 text-[10px] font-bold uppercase tracking-wider text-zinc-400 hover:bg-white/5 hover:text-white transition-all"
          >
            {showAllNics ? "Show fewer" : `Show all ${filtered.length}`}
          </button>
        )}
      </InfoCard>

      {data.routes.length > 0 && (
        <InfoCard title={`Routes (${data.routes.length})`} icon={<NetIcon size={12} />}>
          <div className="space-y-1">
            {data.routes.slice(0, 30).map((r, i) => (
              <div key={i} className="flex items-center gap-2 text-[11px] font-mono px-2 py-1 rounded bg-black/30 border border-white/5">
                <span className="text-zinc-200 truncate min-w-0 flex-1">{r.dst}</span>
                {r.via && <span className="text-zinc-500 shrink-0">via {r.via}</span>}
                {r.dev && <span className="text-primary/80 shrink-0">{r.dev}</span>}
              </div>
            ))}
            {data.routes.length > 30 && (
              <div className="text-[10px] text-zinc-500 italic px-2 pt-1">+{data.routes.length - 30} more</div>
            )}
          </div>
        </InfoCard>
      )}

      <FirewallCard fw={data.firewall} />
    </div>
  );
};

// ---------- FIREWALL ----------
// Engine-aware renderer. iptables output is structured per-chain so we parse
// the `Chain NAME (policy X)` headers and let the user collapse the noisy
// ones; nft's output is a nested config-file syntax that's not worth parsing
// for a viewer, so we render it as preformatted text.
interface FwChain { name: string; policy: string; counters: string; rules: string[]; }
function parseIptablesChains(raw: string): FwChain[] {
  const chains: FwChain[] = [];
  let cur: FwChain | null = null;
  for (const line of raw.split(/\r?\n/)) {
    const m = line.match(/^Chain\s+(\S+)\s*(?:\((.*?)\))?\s*$/);
    if (m) {
      if (cur) chains.push(cur);
      const meta = (m[2] || "").trim();
      const polMatch = meta.match(/^policy\s+(\S+)\s*(.*)$/);
      cur = {
        name: m[1],
        policy: polMatch ? polMatch[1] : "",
        counters: polMatch ? polMatch[2].trim() : meta,
        rules: [],
      };
      continue;
    }
    if (!cur) continue;
    // Skip column-header line so we just keep actual rules.
    if (/^\s*num\s+pkts\s+bytes\s+target/.test(line)) continue;
    if (!line.trim()) continue;
    cur.rules.push(line.trimEnd());
  }
  if (cur) chains.push(cur);
  return chains;
}

const FirewallCard = ({ fw }: { fw: FirewallData }) => {
  const [openChains, setOpenChains] = useState<Record<string, boolean>>({});
  const [filter, setFilter] = useState("");

  if (fw.engine === "none") {
    return (
      <InfoCard title="Firewall" icon={<Shield size={12} />}>
        <BannerIcon tone="zinc" icon={<AlertCircle size={14} />}>
          Neither <code className="font-mono px-1">iptables</code> nor <code className="font-mono px-1">nft</code> is installed on this host.
        </BannerIcon>
      </InfoCard>
    );
  }
  if (fw.denied) {
    return (
      <InfoCard
        title={`Firewall · ${fw.engine}`}
        icon={<Shield size={12} />}
        actions={<StatusBadge tone="amber">denied</StatusBadge>}
      >
        <BannerIcon tone="amber" icon={<ShieldAlert size={14} />}>
          Couldn't read <code className="font-mono px-1">{fw.engine}</code> rules — needs root and passwordless sudo isn't configured. Reconnect as root or grant <code className="font-mono px-1">NOPASSWD</code> sudo for this command.
        </BannerIcon>
      </InfoCard>
    );
  }

  // nft engine: just dump the ruleset. Parsing nested nft syntax is overkill
  // for a viewer — admins reading these rules read them in their native form.
  if (fw.engine === "nft") {
    const filtered = filter.trim()
      ? fw.raw.split(/\r?\n/).filter(l => l.toLowerCase().includes(filter.toLowerCase())).join("\n")
      : fw.raw;
    return (
      <InfoCard
        title="Firewall · nftables"
        icon={<Shield size={12} />}
        actions={
          <div className="flex items-center gap-1.5">
            {fw.usedSudo && <StatusBadge tone="zinc">sudo</StatusBadge>}
            <FilterInput value={filter} onChange={setFilter} placeholder="Filter lines" />
          </div>
        }
      >
        <pre className="text-[10.5px] font-mono text-zinc-300 bg-black/30 border border-white/5 rounded p-2 max-h-[60vh] overflow-auto custom-scrollbar select-text whitespace-pre">
{filtered || <span className="text-zinc-600 italic">No matching lines.</span>}
        </pre>
      </InfoCard>
    );
  }

  // iptables engine: split per-chain so users can collapse the noisy ones.
  const chains = parseIptablesChains(fw.raw);
  const totalRules = chains.reduce((n, c) => n + c.rules.length, 0);
  const needle = filter.trim().toLowerCase();
  return (
    <InfoCard
      title={`Firewall · iptables · ${chains.length} chains · ${totalRules} rules`}
      icon={<Shield size={12} />}
      actions={
        <div className="flex items-center gap-1.5">
          {fw.usedSudo && <StatusBadge tone="zinc">sudo</StatusBadge>}
          <FilterInput value={filter} onChange={setFilter} placeholder="Filter rules" />
        </div>
      }
    >
      {chains.length === 0 ? (
        <Empty>No chains reported.</Empty>
      ) : (
        <div className="space-y-1.5">
          {chains.map((c) => {
            const matchingRules = needle
              ? c.rules.filter(r => r.toLowerCase().includes(needle))
              : c.rules;
            // When filtering, hide chains with no matches so the user isn't
            // scrolling past 20 collapsed empty chains looking for the hit.
            if (needle && matchingRules.length === 0 && !c.name.toLowerCase().includes(needle)) return null;
            // Default-open the small ones AND any chain whose default state is
            // DROP/REJECT — those are the ones the user most likely cares
            // about because they're actively blocking traffic.
            const isInteresting = c.policy === "DROP" || c.policy === "REJECT";
            const explicitlyOpen = openChains[c.name];
            const open = explicitlyOpen !== undefined
              ? explicitlyOpen
              : (matchingRules.length <= 8 || isInteresting || !!needle);
            const polTone = c.policy === "DROP" || c.policy === "REJECT" ? "red"
              : c.policy === "ACCEPT" ? "green" : "zinc";
            return (
              <div key={c.name} className="bg-black/30 border border-white/5 rounded-lg overflow-hidden">
                <button
                  onClick={() => setOpenChains(prev => ({ ...prev, [c.name]: !open }))}
                  className="w-full px-2.5 py-1.5 flex items-center gap-2 text-left hover:bg-white/[0.03] transition-all"
                >
                  <ChevronRightIcon open={open} />
                  <span className="text-[11px] font-mono font-bold text-white truncate">{c.name}</span>
                  {c.policy && <StatusBadge tone={polTone as any}>{c.policy}</StatusBadge>}
                  <span className="text-[9.5px] text-zinc-500 ml-auto shrink-0">
                    {matchingRules.length}{needle && matchingRules.length !== c.rules.length ? `/${c.rules.length}` : ""} rule{matchingRules.length === 1 ? "" : "s"}
                  </span>
                </button>
                {open && (matchingRules.length === 0 ? (
                  <div className="px-3 py-2 text-[10px] text-zinc-600 italic border-t border-white/5">empty</div>
                ) : (
                  <div className="border-t border-white/5 max-h-[40vh] overflow-auto custom-scrollbar">
                    <pre className="text-[10.5px] font-mono text-zinc-300 px-2.5 py-1.5 select-text whitespace-pre">{matchingRules.join("\n")}</pre>
                  </div>
                ))}
              </div>
            );
          })}
        </div>
      )}
    </InfoCard>
  );
};

// Inline chevron used by the firewall chain disclosure. Kept local because
// the rotation is the only behavior — pulling in a button-with-chevron from
// somewhere else would be heavier than this 6-line component.
const ChevronRightIcon = ({ open }: { open: boolean }) => (
  <svg width="10" height="10" viewBox="0 0 24 24" fill="none" stroke="currentColor"
       strokeWidth="2.5" className={`text-zinc-500 transition-transform shrink-0 ${open ? "rotate-90" : ""}`}>
    <polyline points="9 18 15 12 9 6" />
  </svg>
);

const PortsTab = ({ data }: { data: PortsData }) => {
  const [filter, setFilter] = useState("");
  const [protoFilter, setProtoFilter] = useState<"all" | "tcp" | "udp">("all");

  const filtered = useMemo(() => {
    let rows = data.rows;
    if (protoFilter !== "all") rows = rows.filter(r => r.proto === protoFilter);
    if (filter.trim()) {
      const f = filter.toLowerCase();
      rows = rows.filter(r =>
        r.localPort.includes(f) ||
        r.localAddr.toLowerCase().includes(f) ||
        r.process.toLowerCase().includes(f) ||
        r.pid.includes(f)
      );
    }
    return rows;
  }, [data.rows, filter, protoFilter]);

  // Group by proto for stable, easy-to-scan output.
  const tcp = filtered.filter(r => r.proto === "tcp");
  const udp = filtered.filter(r => r.proto === "udp");

  if (!data.available) {
    return (
      <BannerIcon tone="amber" icon={<AlertTriangle size={14} />}>
        Neither <code className="font-mono px-1">ss</code> nor <code className="font-mono px-1">netstat</code> is available. Install <code className="font-mono px-1">iproute2</code> on the host to enable this tab.
      </BannerIcon>
    );
  }

  return (
    <div className="space-y-3">
      {data.partiallyHidden && (
        <BannerIcon tone="amber" icon={<ShieldAlert size={14} />}>
          Some process names are hidden because your user doesn't own them. Reconnect as root or grant NOPASSWD sudo to see all.
        </BannerIcon>
      )}

      <InfoCard
        title={`Listening · ${tcp.length} tcp · ${udp.length} udp`}
        icon={<Plug size={12} />}
        actions={
          <div className="flex items-center gap-1.5">
            <ProtoPill active={protoFilter === "all"} onClick={() => setProtoFilter("all")}>All</ProtoPill>
            <ProtoPill active={protoFilter === "tcp"} onClick={() => setProtoFilter("tcp")}>TCP</ProtoPill>
            <ProtoPill active={protoFilter === "udp"} onClick={() => setProtoFilter("udp")}>UDP</ProtoPill>
            <FilterInput value={filter} onChange={setFilter} placeholder="Port / address / process" />
          </div>
        }
      >
        {filtered.length === 0 ? (
          <Empty>{data.rows.length === 0 ? "No listening sockets reported." : "No matches."}</Empty>
        ) : (
          <div className="overflow-x-auto -mx-3 px-3">
            <table className="w-full text-[11px] font-mono">
              <thead>
                <tr className="text-zinc-500 text-[9px] font-bold uppercase tracking-wider border-b border-white/5">
                  <th className="text-left py-1.5 pr-2 w-10">Proto</th>
                  <th className="text-right py-1.5 pr-2 w-16">Port</th>
                  <th className="text-left py-1.5 pr-2">Address</th>
                  <th className="text-left py-1.5 pr-2">Process</th>
                  <th className="text-right py-1.5 pr-2 w-14">PID</th>
                  <th className="text-left py-1.5">State</th>
                </tr>
              </thead>
              <tbody>
                {filtered.map((r, i) => (
                  <tr key={i} className="border-b border-white/[0.02] hover:bg-white/[0.02]">
                    <td className="py-1 pr-2">
                      <span className={`text-[9px] px-1.5 py-0.5 rounded font-bold uppercase ${r.proto === "tcp" ? "bg-sky-500/15 text-sky-300 border border-sky-500/30" : "bg-amber-500/15 text-amber-300 border border-amber-500/30"}`}>{r.proto}</span>
                    </td>
                    <td className="py-1 pr-2 text-right text-white font-bold">{r.localPort || "—"}</td>
                    <td className="py-1 pr-2 text-zinc-300 truncate max-w-[120px]">{r.localAddr || "—"}</td>
                    <td className="py-1 pr-2 text-zinc-200 truncate max-w-[140px]">
                      {r.process || <span className="text-zinc-600 italic">hidden</span>}
                    </td>
                    <td className="py-1 pr-2 text-right text-zinc-500">{r.pid || "—"}</td>
                    <td className="py-1 text-zinc-500 text-[10px]">{r.state || "—"}</td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
        )}
      </InfoCard>
    </div>
  );
};

const ServicesTab = ({ sessionId, data, onRefresh }: { sessionId: string; data: { available: boolean; rows: ServiceRow[] }; onRefresh: () => void }) => {
  const [filter, setFilter] = useState("");
  const [stateFilter, setStateFilter] = useState<"all" | "active" | "inactive" | "failed">("active");
  const [busy, setBusy] = useState<Record<string, boolean>>({});
  const [lastResult, setLastResult] = useState<Record<string, { ok: boolean; msg: string; sudo: boolean } | undefined>>({});

  const filtered = useMemo(() => {
    let rows = data.rows;
    if (stateFilter !== "all") rows = rows.filter(r => r.active === stateFilter);
    if (filter.trim()) {
      const f = filter.toLowerCase();
      rows = rows.filter(r => r.unit.toLowerCase().includes(f) || r.desc.toLowerCase().includes(f));
    }
    // Active first when "all" is on, then alphabetical.
    rows = [...rows].sort((a, b) => {
      const pri = (s: string) => s === "active" ? 0 : s === "activating" ? 1 : s === "failed" ? 2 : 3;
      const d = pri(a.active) - pri(b.active);
      return d !== 0 ? d : a.unit.localeCompare(b.unit);
    });
    return rows;
  }, [data.rows, filter, stateFilter]);

  if (!data.available) {
    return (
      <BannerIcon tone="amber" icon={<AlertTriangle size={14} />}>
        <code className="font-mono px-1">systemctl</code> not available. This server likely uses a different init system (e.g. OpenRC, runit).
      </BannerIcon>
    );
  }

  const counts = useMemo(() => {
    const c = { active: 0, inactive: 0, failed: 0, other: 0 };
    for (const r of data.rows) {
      if (r.active === "active") c.active++;
      else if (r.active === "inactive") c.inactive++;
      else if (r.active === "failed") c.failed++;
      else c.other++;
    }
    return c;
  }, [data.rows]);

  const performAction = async (unit: string, action: "start" | "stop" | "restart") => {
    setBusy(b => ({ ...b, [unit]: true }));
    setLastResult(r => ({ ...r, [unit]: undefined }));
    try {
      const r = await invoke<SystemctlResult>("ssh_systemctl_action", { sessionId, unit, action });
      setLastResult(prev => ({
        ...prev,
        [unit]: {
          ok: r.success,
          msg: r.success ? `${action} succeeded${r.used_sudo ? " (via sudo)" : ""}` : (r.stdout || `${action} failed (exit ${r.exit_code})`),
          sudo: r.used_sudo,
        },
      }));
      if (r.success) {
        // Refresh service list so state badges update; small delay so
        // the unit has time to settle (start/stop can be async).
        setTimeout(onRefresh, 800);
      }
    } catch (e: any) {
      const msg = typeof e === "string" ? e : (e?.message || `${action} failed`);
      setLastResult(prev => ({ ...prev, [unit]: { ok: false, msg, sudo: false } }));
    } finally {
      setBusy(b => ({ ...b, [unit]: false }));
    }
  };

  return (
    <div className="space-y-3">
      <InfoCard
        title="Services"
        icon={<Cog size={12} />}
        actions={
          <FilterInput value={filter} onChange={setFilter} placeholder="Filter…" />
        }
      >
        <div className="flex items-center gap-1.5 mb-2 flex-wrap">
          <CountPill active={stateFilter === "active"}   onClick={() => setStateFilter("active")}   tone="green"  count={counts.active}>active</CountPill>
          <CountPill active={stateFilter === "inactive"} onClick={() => setStateFilter("inactive")} tone="zinc"   count={counts.inactive}>inactive</CountPill>
          <CountPill active={stateFilter === "failed"}   onClick={() => setStateFilter("failed")}   tone="red"    count={counts.failed}>failed</CountPill>
          <CountPill active={stateFilter === "all"}      onClick={() => setStateFilter("all")}      tone="primary" count={data.rows.length}>all</CountPill>
        </div>

        {filtered.length === 0 ? (
          <Empty>No matching services.</Empty>
        ) : (
          <div className="space-y-1.5 max-h-[60vh] overflow-y-auto custom-scrollbar pr-1">
            {filtered.map((s, i) => {
              const isActive = s.active === "active";
              const result = lastResult[s.unit];
              const isBusy = !!busy[s.unit];
              return (
                <div key={`${s.unit}-${i}`} className="bg-black/30 border border-white/5 rounded-lg p-2">
                  <div className="flex items-center justify-between gap-2">
                    <div className="min-w-0 flex-1">
                      <div className="flex items-center gap-2">
                        <span className="text-[11px] font-mono font-bold text-white truncate">{s.unit}</span>
                        <StatusBadge tone={
                          s.active === "active" ? "green"
                          : s.active === "failed" ? "red"
                          : s.active === "activating" ? "amber"
                          : "zinc"
                        }>{s.active}</StatusBadge>
                        {s.sub && s.sub !== s.active && (
                          <span className="text-[9px] text-zinc-500 uppercase">{s.sub}</span>
                        )}
                      </div>
                      {s.desc && <div className="text-[10px] text-zinc-500 mt-0.5 truncate">{s.desc}</div>}
                    </div>
                    <div className="flex items-center gap-1 shrink-0">
                      {!isActive && (
                        <ActionBtn title="Start" disabled={isBusy} onClick={() => performAction(s.unit, "start")}>
                          <Play size={11} />
                        </ActionBtn>
                      )}
                      {isActive && (
                        <ActionBtn title="Stop" disabled={isBusy} tone="amber" onClick={() => performAction(s.unit, "stop")}>
                          <Square size={11} />
                        </ActionBtn>
                      )}
                      <ActionBtn title="Restart" disabled={isBusy} tone="primary" onClick={() => performAction(s.unit, "restart")}>
                        {isBusy ? <Loader2 size={11} className="animate-spin" /> : <RotateCw size={11} />}
                      </ActionBtn>
                    </div>
                  </div>
                  {result && (
                    <div className={`mt-1.5 text-[10px] px-2 py-1 rounded ${result.ok ? "bg-emerald-500/10 text-emerald-300 border border-emerald-500/20" : "bg-red-500/10 text-red-300 border border-red-500/20"}`}>
                      <span className="font-bold uppercase tracking-wider mr-1">{result.ok ? "OK" : "Error"}</span>
                      <span className="font-mono whitespace-pre-wrap break-words">{result.msg}</span>
                    </div>
                  )}
                </div>
              );
            })}
          </div>
        )}
      </InfoCard>
    </div>
  );
};

// Docker tab moved out — see DockerTab.tsx.

// ============== Reusable bits ==============

interface InfoCardProps {
  title: string; icon?: React.ReactNode; actions?: React.ReactNode; children: React.ReactNode;
  // Hooks for cards that need to participate in a flex layout (e.g. the Disks
  // card on the Overview tab grows to fill the remaining vertical space and
  // scrolls its body internally so the page never grows a global scrollbar).
  className?: string;
  bodyClassName?: string;
}
const InfoCard = ({ title, icon, actions, children, className = "", bodyClassName = "" }: InfoCardProps) => (
  <section className={`bg-[#121215] border border-white/5 rounded-xl overflow-hidden ${className}`}>
    <div className="px-3 py-2 border-b border-white/5 bg-white/[0.02] flex items-center gap-2 flex-wrap">
      <span className="text-zinc-500">{icon}</span>
      <span className="text-[10px] font-bold uppercase tracking-wider text-zinc-400 select-text">{title}</span>
      {actions && <div className="ml-auto">{actions}</div>}
    </div>
    <div className={`p-3 select-text ${bodyClassName}`}>{children}</div>
  </section>
);

interface KVProps { label: string; value: string; icon?: React.ReactNode; }
const KV = ({ label, value, icon }: KVProps) => (
  <div className="flex items-baseline justify-between gap-3 py-1 first:pt-0 last:pb-0 border-b border-white/[0.03] last:border-0">
    <span className="text-[10px] font-bold uppercase tracking-wider text-zinc-500 shrink-0 flex items-center gap-1">
      {icon}{label}
    </span>
    <span className="text-[11px] font-mono text-zinc-200 text-right truncate select-text">{value}</span>
  </div>
);

interface MeterProps { label: string; used: number; total: number; compact?: boolean; }
const Meter = ({ label, used, total, compact }: MeterProps) => {
  const pct = total > 0 ? Math.min(100, Math.max(0, (used / total) * 100)) : 0;
  const tone = pct >= 90 ? "bg-red-500" : pct >= 70 ? "bg-amber-500" : "bg-emerald-500";
  return (
    <div className={compact ? "" : "py-1 first:pt-0 last:pb-0"}>
      {label && (
        <div className="flex items-baseline justify-between text-[10px] mb-1">
          <span className="font-bold uppercase tracking-wider text-zinc-500">{label}</span>
          <span className="font-mono text-zinc-300 select-text">{formatBytes(used)} <span className="text-zinc-500">/ {formatBytes(total)}</span></span>
        </div>
      )}
      {!label && (
        <div className="flex items-baseline justify-between text-[10px] mb-1">
          <span className="font-mono text-zinc-400 select-text">{formatBytes(used)} used</span>
          <span className="font-mono text-zinc-500 select-text">{formatBytes(total)} total · {pct.toFixed(0)}%</span>
        </div>
      )}
      <div className="h-1.5 bg-black/40 rounded-full overflow-hidden">
        <div className={`h-full ${tone} transition-all`} style={{ width: `${pct}%` }} />
      </div>
    </div>
  );
};

const StatusBadge = ({ tone, children }: { tone: "green" | "amber" | "red" | "zinc"; children: React.ReactNode }) => {
  const cls = tone === "green" ? "bg-emerald-500/15 text-emerald-300 border-emerald-500/30"
    : tone === "amber" ? "bg-amber-500/15 text-amber-300 border-amber-500/30"
    : tone === "red"   ? "bg-red-500/15 text-red-300 border-red-500/30"
    : "bg-zinc-500/10 text-zinc-400 border-zinc-500/20";
  return <span className={`shrink-0 text-[9px] px-1.5 py-0.5 rounded font-bold uppercase tracking-wider border ${cls}`}>{children}</span>;
};

const FilterInput = ({ value, onChange, placeholder }: { value: string; onChange: (s: string) => void; placeholder?: string }) => (
  <div className="relative">
    <Search size={11} className="absolute left-2 top-1/2 -translate-y-1/2 text-zinc-600 pointer-events-none" />
    <input
      type="text"
      value={value}
      onChange={(e) => onChange(e.target.value)}
      placeholder={placeholder}
      className="h-7 pl-7 pr-2 text-[10px] bg-black/40 border border-white/10 rounded-md text-zinc-200 placeholder:text-zinc-600 focus:border-primary/50 outline-none w-44"
    />
  </div>
);

const ProtoPill = ({ active, onClick, children }: { active: boolean; onClick: () => void; children: React.ReactNode }) => (
  <button
    onClick={onClick}
    className={`h-9 sm:h-7 px-3 sm:px-2 rounded-md text-[10.5px] font-bold uppercase tracking-wider transition-all border ${active ? "bg-primary/15 text-primary border-primary/30" : "bg-white/[0.04] text-zinc-400 border-white/10 hover:bg-white/[0.08] hover:text-white"}`}
  >
    {children}
  </button>
);

const CountPill = ({ active, onClick, tone, count, children }: { active: boolean; onClick: () => void; tone: "green" | "zinc" | "red" | "primary"; count: number; children: React.ReactNode }) => {
  const baseTone =
    tone === "green" ? (active ? "bg-emerald-500/15 text-emerald-300 border-emerald-500/30" : "bg-white/[0.04] text-zinc-400 border-white/10")
    : tone === "red" ? (active ? "bg-red-500/15 text-red-300 border-red-500/30" : "bg-white/[0.04] text-zinc-400 border-white/10")
    : tone === "primary" ? (active ? "bg-primary/15 text-primary border-primary/30" : "bg-white/[0.04] text-zinc-400 border-white/10")
    : (active ? "bg-zinc-500/20 text-zinc-200 border-zinc-500/30" : "bg-white/[0.04] text-zinc-400 border-white/10");
  return (
    <button
      onClick={onClick}
      className={`h-9 sm:h-7 px-3 sm:px-2 rounded-md text-[10.5px] font-bold uppercase tracking-wider transition-all border flex items-center gap-1.5 ${baseTone} hover:brightness-110`}
    >
      <span>{children}</span>
      <span className="text-[9.5px] opacity-70">{count}</span>
    </button>
  );
};

const ActionBtn = ({ onClick, disabled, title, tone, children }: { onClick: () => void; disabled?: boolean; title: string; tone?: "amber" | "primary"; children: React.ReactNode }) => {
  const cls = tone === "amber" ? "text-amber-300 hover:bg-amber-500/10 border-amber-500/20"
    : tone === "primary" ? "text-primary hover:bg-primary/10 border-primary/20"
    : "text-emerald-300 hover:bg-emerald-500/10 border-emerald-500/20";
  return (
    <button
      onClick={onClick}
      disabled={disabled}
      title={title}
      className={`h-8 w-8 sm:h-7 sm:w-7 rounded-md border bg-white/[0.04] flex items-center justify-center transition-all disabled:opacity-40 disabled:cursor-not-allowed ${cls}`}
    >
      {children}
    </button>
  );
};

const BannerIcon = ({ tone, icon, children }: { tone: "amber" | "zinc" | "red"; icon: React.ReactNode; children: React.ReactNode }) => {
  const cls = tone === "amber" ? "bg-amber-500/5 border-amber-500/20 text-amber-300"
    : tone === "red"   ? "bg-red-500/5 border-red-500/20 text-red-300"
    : "bg-zinc-800/30 border-white/5 text-zinc-300";
  return (
    <div className={`flex items-start gap-2 px-3 py-2 rounded-lg border text-[11px] ${cls}`}>
      <span className="mt-0.5 shrink-0">{icon}</span>
      <div className="select-text">{children}</div>
    </div>
  );
};

const Empty = ({ children }: { children: React.ReactNode }) => (
  <div className="text-[11px] text-zinc-500 italic">{children}</div>
);

export default InfoPanel;
