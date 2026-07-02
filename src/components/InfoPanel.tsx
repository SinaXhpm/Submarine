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
import { useConfirm } from "../ui/confirm";
import CopyValue from "./CopyValue";

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
//
// The backend now emits a per-chain summary (not the full ruleset) so first
// paint stays under 2 KB even on hosts with thousands of rules. The full
// rules for a specific chain are pulled lazily via `ssh_iptables_chain` /
// `ssh_nft_chain` when the user expands one.
type FwEngine = "iptables" | "nft" | "none";
interface FwChainSummary {
  // Common
  name: string;
  count: number;
  // iptables: `table` is the netfilter table name (filter/nat/mangle/raw);
  //           `family` is unused (empty string); `policy` is ACCEPT/DROP/-.
  // nft:      `family` is ip/ip6/inet/…; `table` is the nft table name;
  //           `policy` holds the chain type (filter/nat/route) — reused so
  //           the FirewallCard render stays type-uniform.
  table: string;
  family: string;
  policy: string;
}
interface FirewallData {
  engine: FwEngine;
  usedSudo: boolean;
  available: boolean;
  denied: boolean;        // engine present but ruleset unreadable (perm denied)
  chains: FwChainSummary[];
}
function parseFirewall(section: string): FirewallData {
  const empty = (): FirewallData => ({ engine: "none", usedSudo: false, available: false, denied: false, chains: [] });
  const trimmed = section.trim();
  if (!trimmed) return empty();
  const nl = trimmed.indexOf("\n");
  const header = nl >= 0 ? trimmed.slice(0, nl).trim() : trimmed;
  const body = nl >= 0 ? trimmed.slice(nl + 1) : "";
  const m = header.match(/^FW:(.+)$/);
  if (!m) return empty();
  const tag = m[1];
  if (tag === "none") return empty();
  if (tag.endsWith("-denied")) {
    const eng = tag.startsWith("iptables") ? "iptables" : "nft";
    return { engine: eng, usedSudo: false, available: false, denied: true, chains: [] };
  }
  const usedSudo = tag.endsWith("-sudo");
  const eng: FwEngine = tag.startsWith("iptables") ? "iptables" : "nft";
  const chains: FwChainSummary[] = [];
  for (const rawLine of body.split(/\r?\n/)) {
    const line = rawLine.trim();
    if (!line) continue;
    const parts = line.split("|");
    if (eng === "iptables") {
      // table|chain|policy|count
      if (parts.length < 4) continue;
      const [table, name, policy, countStr] = parts;
      const count = parseInt(countStr, 10);
      if (!name || !Number.isFinite(count)) continue;
      chains.push({ name, count, table, family: "", policy });
    } else {
      // family|table|chain|type|count
      if (parts.length < 5) continue;
      const [family, table, name, type, countStr] = parts;
      const count = parseInt(countStr, 10);
      if (!name || !Number.isFinite(count)) continue;
      chains.push({ name, count, table, family, policy: type });
    }
  }
  return { engine: eng, usedSudo, available: true, denied: false, chains };
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

  // Per-section token counter. We can't cancel an SSH exec channel from
  // the frontend (that would need a backend-side abort handle, out of
  // scope here), but we CAN discard the result when a stale probe returns.
  // Bumping the token on tab-switch means an in-flight probe for the old
  // tab won't stomp on the current tab's state, and won't leave the old
  // tab stuck in loading=true if the user switches back before it
  // resolves. Net effect: no visible bandwidth waste from stale updates.
  const tokenRef = useRef<Record<ProbeSubTab, number>>({
    overview: 0, network: 0, ports: 0, services: 0,
  });

  const fetchSection = async (which: SubTab) => {
    if (disabled) return;
    if (which === "docker") return; // handled inside DockerTab
    const setter = stateMap[which as ProbeSubTab][1] as (s: any) => void;
    const myToken = ++tokenRef.current[which as ProbeSubTab];
    setter((prev: TabState<any>) => ({ ...prev, loading: true, error: null }));
    try {
      const r = await invoke<SectionResult>("ssh_info_probe_section", { sessionId, section: which });
      if (tokenRef.current[which as ProbeSubTab] !== myToken) return;
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
      if (tokenRef.current[which as ProbeSubTab] !== myToken) return;
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

  // On tab switch, invalidate in-flight probes for the tab we're leaving
  // by bumping every section's token in the cleanup. Placed in cleanup
  // (not the effect body) so we invalidate BEFORE the next lazy-fetch
  // effect runs and stakes a fresh token — otherwise we'd cancel the
  // just-started probe on the tab the user just landed on.
  useEffect(() => {
    return () => {
      for (const k of Object.keys(tokenRef.current) as ProbeSubTab[]) {
        tokenRef.current[k]++;
      }
    };
  }, [tab]);

  // ---------- Tab strip ----------
  // ScrollableTabs wraps overflow onto a second row instead of hiding tabs
  // behind a scroll. On narrow widths (split panes, phone) this means a tab
  // strip can take two rows — preferable to the previous chevron-scroll
  // pattern which hid tabs behind interaction the user couldn't discover.
  const SubTabStrip = (
    <div className="shrink-0 border-b border-white/5 bg-white/[0.02] px-3 py-2">
      <ScrollableTabs>
        {TABS.map(t => {
          const Icon = t.icon;
          return (
            <button
              key={t.id}
              onClick={() => setTab(t.id)}
              className={`${subTabBase} ${tab === t.id ? subTabActive : subTabIdle}`}
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
        {tab !== "docker" && (
          <button
            onClick={() => fetchSection(tab)}
            disabled={activeState.loading || disabled}
            title="Refresh this tab"
            className="h-9 sm:h-7 px-3 sm:px-2.5 rounded-md flex items-center gap-1.5 text-[10.5px] font-bold uppercase tracking-wider text-zinc-300 bg-white/[0.04] border border-white/10 hover:bg-white/[0.08] hover:text-white disabled:opacity-40 disabled:cursor-not-allowed transition-all"
          >
            {activeState.loading ? <Loader2 size={12} className="animate-spin" /> : <RefreshCw size={12} />}
            <span>Refresh</span>
          </button>
        )}
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
        {tab === "network"  && network.data  && <NetworkTab sessionId={sessionId} data={network.data} />}
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
          <KV label="Hostname" value={data.hostname || "—"} copyable />
          <KV label="OS" value={data.os || "—"} copyable />
          <KV label="Kernel" value={data.uname || "—"} copyable />
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

const NetworkTab = ({ sessionId, data }: { sessionId: string; data: NetworkData }) => {
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
                <div className="text-[10px] text-zinc-500 font-mono mb-1 truncate flex items-center gap-1">
                  {n.mac ? <CopyValue value={n.mac}>{n.mac}</CopyValue> : "—"}
                  <span>· MTU {n.mtu || "—"}</span>
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
                        <span className="truncate min-w-0">
                          <CopyValue value={a.local}>
                            <span>{a.local}<span className="text-zinc-500">/{a.prefixlen}</span></span>
                          </CopyValue>
                        </span>
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

      <FirewallCard sessionId={sessionId} fw={data.firewall} />
    </div>
  );
};

// ---------- FIREWALL ----------
// Two-stage render: the network probe returns per-chain summaries (chain
// name + rule count + policy/type). Rendering starts collapsed so we send
// ~1-2 KB on first paint instead of the 50-500 KB the raw ruleset would
// cost. Expanding a chain triggers a single-chain fetch via the dedicated
// ssh_iptables_chain / ssh_nft_chain commands; the response is cached in
// component state so re-collapsing / re-expanding is free.

interface FirewallCardProps { sessionId: string; fw: FirewallData; }
const FirewallCard = ({ sessionId, fw }: FirewallCardProps) => {
  const [openChains, setOpenChains] = useState<Record<string, boolean>>({});
  const [chainRules, setChainRules] = useState<Record<string, { loading: boolean; error: string | null; text: string | null }>>({});
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

  // Composite key: chain names aren't globally unique across tables/families
  // (e.g. FORWARD exists in both `filter` and `mangle`), so key by
  // engine + fully-qualified location + name so expanded state doesn't
  // spill between chains that share a display name.
  const keyOf = (c: FwChainSummary) =>
    fw.engine === "iptables" ? `ipt:${c.table}:${c.name}` : `nft:${c.family}:${c.table}:${c.name}`;

  const fetchChain = async (c: FwChainSummary) => {
    const k = keyOf(c);
    const existing = chainRules[k];
    if (existing && (existing.loading || existing.text !== null)) return;
    setChainRules(prev => ({ ...prev, [k]: { loading: true, error: null, text: null } }));
    try {
      const text = fw.engine === "iptables"
        ? await invoke<string>("ssh_iptables_chain", { sessionId, table: c.table, chain: c.name })
        : await invoke<string>("ssh_nft_chain", { sessionId, family: c.family, table: c.table, chain: c.name });
      setChainRules(prev => ({ ...prev, [k]: { loading: false, error: null, text } }));
    } catch (e: any) {
      const msg = typeof e === "string" ? e : (e?.message || "chain fetch failed");
      setChainRules(prev => ({ ...prev, [k]: { loading: false, error: msg, text: null } }));
    }
  };

  const chains = fw.chains;
  const totalRules = chains.reduce((n, c) => n + c.count, 0);
  const needle = filter.trim().toLowerCase();
  const label = fw.engine === "iptables" ? "iptables" : "nftables";

  // Filtering — chain-name / table / family only. We don't have rule text on
  // first paint, so a text-body filter would need to bulk-fetch every chain
  // (defeating the whole optimization). Keep it lightweight: match against
  // the metadata we already have.
  const visibleChains = needle
    ? chains.filter(c =>
        c.name.toLowerCase().includes(needle) ||
        c.table.toLowerCase().includes(needle) ||
        (c.family && c.family.toLowerCase().includes(needle)) ||
        (c.policy && c.policy.toLowerCase().includes(needle))
      )
    : chains;

  return (
    <InfoCard
      title={`Firewall · ${label} · ${chains.length} chains · ${totalRules} rules`}
      icon={<Shield size={12} />}
      actions={
        <div className="flex items-center gap-1.5">
          {fw.usedSudo && <StatusBadge tone="zinc">sudo</StatusBadge>}
          <FilterInput value={filter} onChange={setFilter} placeholder="Filter chains" />
        </div>
      }
    >
      {visibleChains.length === 0 ? (
        <Empty>{chains.length === 0 ? "No chains reported." : "No chains match."}</Empty>
      ) : (
        <div className="space-y-1.5">
          {visibleChains.map((c) => {
            const k = keyOf(c);
            const explicitlyOpen = openChains[k];
            // Only DROP/REJECT chains auto-expand — those are actively
            // blocking traffic so the operator likely wants to see rules
            // immediately. Everything else stays collapsed to preserve the
            // bandwidth win: no chain fetch until the user clicks.
            const isInteresting = c.policy === "DROP" || c.policy === "REJECT";
            const open = explicitlyOpen !== undefined ? explicitlyOpen : (isInteresting && c.count > 0);
            const polTone = c.policy === "DROP" || c.policy === "REJECT" ? "red"
              : c.policy === "ACCEPT" ? "green" : "zinc";
            const detail = chainRules[k];
            // Location suffix: for iptables it's `[table]`, for nft it's `[family/table]`.
            const loc = fw.engine === "iptables" ? c.table : `${c.family}/${c.table}`;
            return (
              <div key={k} className="bg-black/30 border border-white/5 rounded-lg overflow-hidden">
                <button
                  onClick={() => {
                    const next = !open;
                    setOpenChains(prev => ({ ...prev, [k]: next }));
                    if (next && c.count > 0) fetchChain(c);
                  }}
                  className="w-full px-2.5 py-1.5 flex items-center gap-2 text-left hover:bg-white/[0.03] transition-all"
                >
                  <ChevronRightIcon open={open} />
                  <span className="text-[11px] font-mono font-bold text-white truncate">{c.name}</span>
                  <span className="text-[9.5px] text-zinc-500 shrink-0 font-mono">{loc}</span>
                  {c.policy && c.policy !== "-" && <StatusBadge tone={polTone as any}>{c.policy}</StatusBadge>}
                  <span className="text-[9.5px] text-zinc-500 ml-auto shrink-0">
                    {c.count} rule{c.count === 1 ? "" : "s"}
                  </span>
                </button>
                {open && (
                  c.count === 0 ? (
                    <div className="px-3 py-2 text-[10px] text-zinc-600 italic border-t border-white/5">empty</div>
                  ) : detail?.loading ? (
                    <div className="px-3 py-2 text-[10px] text-zinc-500 border-t border-white/5 flex items-center gap-2">
                      <Loader2 size={11} className="animate-spin" /> Loading…
                    </div>
                  ) : detail?.error ? (
                    <div className="px-3 py-2 text-[10px] text-red-300 border-t border-white/5 font-mono">{detail.error}</div>
                  ) : detail?.text != null ? (
                    <div className="border-t border-white/5 max-h-[40vh] overflow-auto custom-scrollbar">
                      <pre className="text-[10.5px] font-mono text-zinc-300 px-2.5 py-1.5 select-text whitespace-pre">
                        {detail.text.trim() || <span className="text-zinc-600 italic">empty</span>}
                      </pre>
                    </div>
                  ) : null
                )}
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
                    <td className="py-1 pr-2 text-right text-white font-bold">
                      {r.localPort ? <CopyValue value={r.localPort}>{r.localPort}</CopyValue> : "—"}
                    </td>
                    <td className="py-1 pr-2 text-zinc-300 truncate" title={r.localAddr}>
                      {r.localAddr ? <CopyValue value={r.localAddr}>{r.localAddr}</CopyValue> : "—"}
                    </td>
                    <td className="py-1 pr-2 text-zinc-200 truncate" title={r.process}>
                      {r.process ? <CopyValue value={r.process}>{r.process}</CopyValue> : <span className="text-zinc-600 italic">hidden</span>}
                    </td>
                    <td className="py-1 pr-2 text-right text-zinc-500">
                      {r.pid ? <CopyValue value={r.pid}>{r.pid}</CopyValue> : "—"}
                    </td>
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
  const confirm = useConfirm();
  // First-arrival latch: once we've seen the first data payload we stop
  // clobbering the user's manual filter selection.
  const didSetInitialRef = useRef(false);

  const filtered = useMemo(() => {
    let rows = data.rows;
    if (stateFilter !== "all") rows = rows.filter(r => r.active === stateFilter);
    if (filter.trim()) {
      const f = filter.toLowerCase();
      rows = rows.filter(r => r.unit.toLowerCase().includes(f) || r.desc.toLowerCase().includes(f));
    }
    // When filter is "all", surface failed units at the top so operators
    // spot problems first; otherwise use the standard active-first order.
    rows = [...rows].sort((a, b) => {
      const pri = stateFilter === "all"
        ? (s: string) => s === "failed" ? 0 : s === "active" ? 1 : s === "activating" ? 2 : 3
        : (s: string) => s === "active" ? 0 : s === "activating" ? 1 : s === "failed" ? 2 : 3;
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

  // On first data arrival, jump straight to "failed" if any failed units
  // exist so operators see problems immediately; otherwise default to "all".
  // We only run this once (didSetInitialRef) so subsequent refreshes don't
  // clobber the user's manual pill selection.
  useEffect(() => {
    if (didSetInitialRef.current) return;
    if (!data.rows.length) return;
    didSetInitialRef.current = true;
    if (counts.failed > 0) setStateFilter("failed");
    else setStateFilter("all");
  }, [data.rows, counts.failed]);

  const performAction = async (unit: string, action: "start" | "stop" | "restart") => {
    if (action === "stop" || action === "restart") {
      const ok = await confirm({
        title: action === "stop" ? `Stop ${unit}?` : `Restart ${unit}?`,
        message: "This will disconnect anyone currently connected.",
        destructive: true,
        okLabel: action === "stop" ? "Stop" : "Restart",
      });
      if (!ok) return;
    }
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
                        <span className="text-[11px] font-mono font-bold text-white truncate min-w-0">
                          <CopyValue value={s.unit}>{s.unit}</CopyValue>
                        </span>
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
                      {s.desc && <div className="text-[10px] text-zinc-500 mt-0.5 truncate" title={s.desc}>{s.desc}</div>}
                    </div>
                    <div className="flex items-center gap-1 shrink-0">
                      {!isActive && (
                        <ActionBtn title="Start" disabled={isBusy} onClick={() => performAction(s.unit, "start")}>
                          <Play size={11} />
                        </ActionBtn>
                      )}
                      {isActive && (
                        <ActionBtn title="Stop" disabled={isBusy} tone="rose" onClick={() => performAction(s.unit, "stop")}>
                          <Square size={11} />
                        </ActionBtn>
                      )}
                      <ActionBtn title="Restart" disabled={isBusy} tone="amber" onClick={() => performAction(s.unit, "restart")}>
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

interface KVProps { label: string; value: string; icon?: React.ReactNode; copyable?: boolean; }
const KV = ({ label, value, icon, copyable }: KVProps) => (
  <div className="flex items-baseline justify-between gap-3 py-1 first:pt-0 last:pb-0 border-b border-white/[0.03] last:border-0">
    <span className="text-[10px] font-bold uppercase tracking-wider text-zinc-500 shrink-0 flex items-center gap-1">
      {icon}{label}
    </span>
    {copyable && value && value !== "—" ? (
      <span className="text-[11px] font-mono text-zinc-200 text-right truncate select-text min-w-0">
        <CopyValue value={value}>{value}</CopyValue>
      </span>
    ) : (
      <span className="text-[11px] font-mono text-zinc-200 text-right truncate select-text">{value}</span>
    )}
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

const ActionBtn = ({ onClick, disabled, title, tone, children }: { onClick: () => void; disabled?: boolean; title: string; tone?: "amber" | "primary" | "rose"; children: React.ReactNode }) => {
  const cls = tone === "amber" ? "text-amber-300 hover:bg-amber-500/10 border-amber-500/20"
    : tone === "primary" ? "text-primary hover:bg-primary/10 border-primary/20"
    : tone === "rose" ? "text-rose-300 bg-rose-500/15 hover:bg-rose-500/25 border-rose-500/30"
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
