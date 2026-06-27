import { useEffect, useMemo, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import {
  RefreshCw, Loader2, Play, Square, RotateCw, Pause, X, ChevronRight,
  Container as ContainerIcon, Image as ImageIcon, Database, Network as NetIcon,
  FileText, Trash2, AlertTriangle, AlertCircle, ShieldAlert, Terminal,
  Eye, Box, Activity, Search, Copy, Check,
} from "lucide-react";

interface DockerTabProps {
  sessionId: string;
  disabled?: boolean;
  onOpenContainerTerminal: (containerName: string, useSudo: boolean) => void;
}

type SubTab = "containers" | "resources" | "compose" | "prune";
type ResourceKind = "images" | "volumes" | "networks";

interface DockerActionResult { success: boolean; output: string; used_sudo: boolean; }
interface DockerTextResult { success: boolean; data: string; used_sudo: boolean; }

interface Container { id: string; image: string; names: string; status: string; state: string; ports: string; createdAt: string; }
interface VolumeRow { name: string; driver: string; mountpoint: string; }
interface ImageRow { repo: string; tag: string; id: string; size: string; createdSince: string; }
interface NetworkRow { id: string; name: string; driver: string; scope: string; }
interface ComposeProject { name: string; status: string; configFiles: string; }
// One row per container that was started by compose. Containers with no
// compose labels are excluded at parse time. `configFile` is the FIRST
// path from the comma-separated list (compose supports `-f a.yml -f b.yml`
// layering; we surface the primary, viewing additional layers is rare).
interface ComposeContainer {
  containerName: string;
  project: string;
  service: string;
  configFile: string;
  state: string;
}
interface ContainerStats { id: string; name: string; cpuPerc: string; memUsage: string; memPerc: string; netIO: string; blockIO: string; pids: string; }

// Parse `--format '{{json .}}'` JSONL output. One JSON object per line.
function parseJsonl<T>(raw: string, mapper: (o: any) => T): T[] {
  const out: T[] = [];
  for (const line of raw.split(/\r?\n/)) {
    const l = line.trim();
    if (!l) continue;
    try { out.push(mapper(JSON.parse(l))); } catch { /* skip */ }
  }
  return out;
}

// Parse the pipe-separated per-container compose data emitted by our
// custom `docker ps --format` string. Lines without a project label
// (i.e. containers not started by compose) are filtered out — they have
// no docker-compose.yml to show.
function parseComposeContainers(raw: string): ComposeContainer[] {
  const out: ComposeContainer[] = [];
  for (const line of raw.split(/\r?\n/)) {
    if (!line.trim()) continue;
    const parts = line.split("|");
    if (parts.length < 5) continue;
    const [name, project, configFiles, service, state] = parts;
    if (!name.trim() || !project.trim()) continue;
    out.push({
      containerName: name.trim(),
      project: project.trim(),
      service: service.trim(),
      // Compose can use multiple `-f` layers; we take the first as primary.
      configFile: (configFiles.split(",")[0] || "").trim(),
      state: state.trim().toLowerCase(),
    });
  }
  return out;
}

// Parse the plain tabular output of `docker compose ls --all`:
//   NAME                STATUS              CONFIG FILES
//   myapp               running(3)          /home/u/myapp/docker-compose.yml
// We detect the header to find column boundaries — robust against varying
// docker versions that pad columns differently. Lines before the header
// (deprecation warnings, etc.) are skipped silently.
function parseComposeText(raw: string): ComposeProject[] {
  const lines = raw.split(/\r?\n/);
  let headerIdx = -1;
  let nameStart = -1, statusStart = -1, configStart = -1;
  for (let i = 0; i < lines.length; i++) {
    const line = lines[i];
    const upper = line.toUpperCase();
    if (upper.startsWith("NAME") && upper.includes("STATUS") && upper.includes("CONFIG FILES")) {
      headerIdx = i;
      nameStart = upper.indexOf("NAME");
      statusStart = upper.indexOf("STATUS");
      configStart = upper.indexOf("CONFIG FILES");
      break;
    }
  }
  if (headerIdx < 0) return [];
  const projects: ComposeProject[] = [];
  for (let i = headerIdx + 1; i < lines.length; i++) {
    const line = lines[i];
    if (!line.trim()) continue;
    const name = line.substring(nameStart, statusStart).trim();
    const status = line.substring(statusStart, configStart).trim();
    const configFiles = line.substring(configStart).trim();
    if (name) projects.push({ name, status, configFiles });
  }
  return projects;
}

function describeError(err: string, ctx?: string): { title: string; hint?: string } {
  const e = (err || "").toLowerCase();
  if (e.includes("session is not connected") || e.includes("session not connected")) {
    return { title: "Session is disconnected", hint: "Reconnect to fetch fresh data." };
  }
  if (e.includes("timed out") || e.includes("timeout")) {
    return { title: "Server didn't respond in time", hint: "Docker may be hung. Try again or check the daemon." };
  }
  if (e.includes("cannot connect to the docker daemon")) {
    return { title: "Cannot reach the Docker daemon", hint: "Add the SSH user to the `docker` group or grant NOPASSWD sudo." };
  }
  if (e.includes("permission denied")) {
    return { title: "Permission denied", hint: ctx === "exec" ? "Container's shell or docker socket can't be accessed." : "Configure NOPASSWD sudo on the host for `docker`." };
  }
  if (e.includes("no such container")) return { title: "Container no longer exists", hint: "It may have been removed since the list was fetched. Refresh." };
  if (e.includes("no such image")) return { title: "Image not found" };
  if (e.includes("no shell found in container")) return { title: "No shell in container", hint: "Distroless or scratch image — interactive exec not possible." };
  return { title: err || "Action failed" };
}

// Mobile-first sizing: h-9 (36 px) keeps the tap target above 32 px on
// phones; the explicit `min-w-[44px]` on icon-only variants guards the
// 44 pt iOS guideline even when the label is a single short word.
const subTabBase = "shrink-0 h-9 sm:h-8 px-3 rounded-md flex items-center gap-1.5 text-[10.5px] font-bold uppercase tracking-wider transition-all";
const subTabIdle = "text-zinc-400 bg-white/[0.04] border border-white/10 hover:bg-white/[0.08] hover:text-white";
const subTabActive = "bg-primary/15 text-primary border border-primary/30 shadow-inner";

interface TabState<T> { loaded: boolean; loading: boolean; error: string | null; data: T | null; }
function fresh<T>(): TabState<T> { return { loaded: false, loading: false, error: null, data: null }; }

const DockerTab = ({ sessionId, disabled, onOpenContainerTerminal }: DockerTabProps) => {
  const [tab, setTab] = useState<SubTab>("containers");

  const [containers, setContainers] = useState<TabState<Container[]>>(fresh);
  const [images, setImages] = useState<TabState<ImageRow[]>>(fresh);
  const [volumes, setVolumes] = useState<TabState<VolumeRow[]>>(fresh);
  const [networks, setNetworks] = useState<TabState<NetworkRow[]>>(fresh);
  const [composeContainers, setComposeContainers] = useState<TabState<ComposeContainer[]>>(fresh);

  const [detailsFor, setDetailsFor] = useState<Container | null>(null);
  const [composeFor, setComposeFor] = useState<ComposeContainer | null>(null);

  const fetchContainers = async () => {
    setContainers(s => ({ ...s, loading: true, error: null }));
    try {
      // Direct `docker ps -a` — fast (~100–300 ms). Don't chain a `docker
      // stats` here: stats has multi-second overhead per container and the
      // user explicitly asked for a quick first paint. Stats only loads
      // when the user opens a container's Stats tab.
      const r = await invoke<DockerTextResult>("ssh_docker_containers", { sessionId });
      if (!r.success) throw new Error(r.data || "container list failed");
      const data = parseJsonl<Container>(r.data, o => ({
        id: o.ID || "",
        image: o.Image || "",
        names: o.Names || "",
        status: o.Status || "",
        state: (o.State || "").toLowerCase(),
        ports: o.Ports || "",
        createdAt: o.CreatedAt || "",
      }));
      setContainers({ loaded: true, loading: false, error: null, data });
    } catch (e: any) {
      setContainers(s => ({ ...s, loading: false, error: typeof e === "string" ? e : (e?.message || "failed") }));
    }
  };

  const fetchImages = async () => {
    setImages(s => ({ ...s, loading: true, error: null }));
    try {
      const r = await invoke<DockerTextResult>("ssh_docker_images_list", { sessionId });
      if (!r.success) throw new Error(r.data || "image list failed");
      const data = parseJsonl<ImageRow>(r.data, o => ({
        repo: o.Repository || "<none>",
        tag: o.Tag || "<none>",
        id: o.ID || "",
        size: o.Size || "",
        createdSince: o.CreatedSince || "",
      }));
      setImages({ loaded: true, loading: false, error: null, data });
    } catch (e: any) {
      setImages(s => ({ ...s, loading: false, error: typeof e === "string" ? e : (e?.message || "failed") }));
    }
  };

  const fetchVolumes = async () => {
    setVolumes(s => ({ ...s, loading: true, error: null }));
    try {
      const r = await invoke<DockerTextResult>("ssh_docker_volumes_list", { sessionId });
      if (!r.success) throw new Error(r.data || "volume list failed");
      const data = parseJsonl<VolumeRow>(r.data, o => ({
        name: o.Name || "",
        driver: o.Driver || "",
        mountpoint: o.Mountpoint || "",
      }));
      setVolumes({ loaded: true, loading: false, error: null, data });
    } catch (e: any) {
      setVolumes(s => ({ ...s, loading: false, error: typeof e === "string" ? e : (e?.message || "failed") }));
    }
  };

  const fetchNetworks = async () => {
    setNetworks(s => ({ ...s, loading: true, error: null }));
    try {
      const r = await invoke<DockerTextResult>("ssh_docker_networks", { sessionId });
      if (!r.success) throw new Error(r.data || "network list failed");
      const data = parseJsonl<NetworkRow>(r.data, o => ({
        id: o.ID || "",
        name: o.Name || "",
        driver: o.Driver || "",
        scope: o.Scope || "",
      }));
      setNetworks({ loaded: true, loading: false, error: null, data });
    } catch (e: any) {
      setNetworks(s => ({ ...s, loading: false, error: typeof e === "string" ? e : (e?.message || "failed") }));
    }
  };

  const [composeRaw, setComposeRaw] = useState<string>("");
  const fetchCompose = async () => {
    setComposeContainers(s => ({ ...s, loading: true, error: null }));
    try {
      const r = await invoke<DockerTextResult>("ssh_docker_compose_per_container", { sessionId });
      if (!r.success) throw new Error(r.data || "compose query failed");
      setComposeRaw(r.data);
      const data = parseComposeContainers(r.data);
      setComposeContainers({ loaded: true, loading: false, error: null, data });
    } catch (e: any) {
      setComposeContainers(s => ({ ...s, loading: false, error: typeof e === "string" ? e : (e?.message || "failed") }));
    }
  };

  // Lazy: fire fetch on first tab activation. Subsequent activations
  // reuse the cached state until Refresh. The "resources" umbrella tab
  // doesn't auto-fetch — its inner segmented control fires the per-kind
  // fetch when the user picks images / volumes / networks, so we never
  // burn a round-trip on a kind they don't actually want to see.
  useEffect(() => {
    if (disabled) return;
    if (tab === "containers" && !containers.loaded && !containers.loading) fetchContainers();
    if (tab === "compose"    && !composeContainers.loaded && !composeContainers.loading) fetchCompose();
    // prune has no fetch — it's an action-only tab.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [tab, disabled, sessionId]);

  const refreshActive = () => {
    if (tab === "containers") fetchContainers();
    else if (tab === "compose") fetchCompose();
    else if (tab === "resources") {
      // Refresh whichever resource the user already loaded; if none yet
      // loaded, defaulting to images is the most useful starting point.
      if (images.loaded) fetchImages();
      if (volumes.loaded) fetchVolumes();
      if (networks.loaded) fetchNetworks();
      if (!images.loaded && !volumes.loaded && !networks.loaded) fetchImages();
    }
  };

  return (
    <div className="space-y-3">
      <div className="flex items-center gap-1.5 overflow-x-auto no-scrollbar -mx-1 px-1 snap-x">
        <SubBtn active={tab === "containers"} onClick={() => setTab("containers")} icon={<ContainerIcon size={11} />}>Containers</SubBtn>
        <SubBtn active={tab === "resources"}  onClick={() => setTab("resources")}  icon={<Database size={11} />}>Resources</SubBtn>
        <SubBtn active={tab === "compose"}    onClick={() => setTab("compose")}    icon={<FileText size={11} />}>Compose</SubBtn>
        <SubBtn active={tab === "prune"}      onClick={() => setTab("prune")}      icon={<Trash2 size={11} />}>Prune</SubBtn>
        <div className="ml-auto">
          {tab !== "prune" && (
            <button
              onClick={refreshActive}
              title="Refresh this tab"
              className="h-7 px-2 rounded-md text-[10px] font-bold uppercase tracking-wider text-zinc-300 bg-white/[0.04] border border-white/10 hover:bg-white/[0.08] hover:text-white flex items-center gap-1.5 transition-all"
            >
              <RefreshCw size={11} /> Refresh
            </button>
          )}
        </div>
      </div>

      {tab === "containers" && (
        <ContainersView
          state={containers}
          sessionId={sessionId}
          onRefresh={fetchContainers}
          onOpenDetails={setDetailsFor}
          onOpenContainerTerminal={onOpenContainerTerminal}
        />
      )}
      {tab === "resources" && (
        <ResourcesView
          images={images}
          volumes={volumes}
          networks={networks}
          fetchImages={fetchImages}
          fetchVolumes={fetchVolumes}
          fetchNetworks={fetchNetworks}
        />
      )}
      {tab === "compose" && (
        <ComposeView
          state={composeContainers}
          rawFallback={composeRaw}
          onOpen={setComposeFor}
        />
      )}
      {tab === "prune" && <PruneView sessionId={sessionId} />}

      {detailsFor && (
        <ContainerDetailsModal
          sessionId={sessionId}
          container={detailsFor}
          onClose={() => setDetailsFor(null)}
          onOpenContainerTerminal={(name, sudo) => {
            setDetailsFor(null);
            onOpenContainerTerminal(name, sudo);
          }}
        />
      )}
      {composeFor && (
        <ComposeViewerModal
          sessionId={sessionId}
          container={composeFor}
          onClose={() => setComposeFor(null)}
        />
      )}
    </div>
  );
};

// ============== Sub-tabs ==============

const ContainersView = ({
  state, sessionId, onRefresh, onOpenDetails, onOpenContainerTerminal,
}: {
  state: TabState<Container[]>;
  sessionId: string;
  onRefresh: () => void;
  onOpenDetails: (c: Container) => void;
  onOpenContainerTerminal: (name: string, useSudo: boolean) => void;
}) => {
  const [filter, setFilter] = useState("");
  const [stateFilter, setStateFilter] = useState<"all" | "running" | "stopped">("running");
  const [busy, setBusy] = useState<Record<string, string>>({});
  const [lastResult, setLastResult] = useState<Record<string, { ok: boolean; msg: string; sudo: boolean } | undefined>>({});

  if (state.loading && !state.data) return <Spinner label="Loading containers…" />;
  if (state.error) return <ErrorBanner err={state.error} />;
  if (!state.data) return null;

  const filtered = state.data.filter(c => {
    if (stateFilter === "running" && c.state !== "running") return false;
    if (stateFilter === "stopped" && c.state === "running") return false;
    if (filter.trim()) {
      const f = filter.toLowerCase();
      if (!c.names.toLowerCase().includes(f) && !c.image.toLowerCase().includes(f) && !c.id.toLowerCase().includes(f)) return false;
    }
    return true;
  });

  const counts = {
    running: state.data.filter(c => c.state === "running").length,
    stopped: state.data.filter(c => c.state !== "running").length,
    all: state.data.length,
  };

  const doAction = async (c: Container, action: string, needsConfirm = false) => {
    if (needsConfirm && !window.confirm(`${action.toUpperCase()} ${c.names}?`)) return;
    setBusy(b => ({ ...b, [c.id]: action }));
    setLastResult(r => ({ ...r, [c.id]: undefined }));
    try {
      const r = await invoke<DockerActionResult>("ssh_docker_container_action", {
        sessionId,
        container: c.names || c.id,
        action,
      });
      setLastResult(prev => ({
        ...prev,
        [c.id]: {
          ok: r.success,
          msg: r.success ? `${action} ok${r.used_sudo ? " (via sudo)" : ""}` : (r.output || `${action} failed`),
          sudo: r.used_sudo,
        },
      }));
      if (r.success) setTimeout(onRefresh, 600);
    } catch (e: any) {
      setLastResult(prev => ({ ...prev, [c.id]: { ok: false, msg: typeof e === "string" ? e : (e?.message || "failed"), sudo: false } }));
    } finally {
      setBusy(b => { const n = { ...b }; delete n[c.id]; return n; });
    }
  };

  return (
    <div className="space-y-2">
      <div className="flex items-center gap-1.5 flex-wrap">
        <CountPill active={stateFilter === "running"} tone="green"   count={counts.running} onClick={() => setStateFilter("running")}>running</CountPill>
        <CountPill active={stateFilter === "stopped"} tone="zinc"    count={counts.stopped} onClick={() => setStateFilter("stopped")}>stopped</CountPill>
        <CountPill active={stateFilter === "all"}     tone="primary" count={counts.all}     onClick={() => setStateFilter("all")}>all</CountPill>
        <div className="ml-auto">
          <FilterInput value={filter} onChange={setFilter} placeholder="Name / image / id" />
        </div>
      </div>

      {filtered.length === 0 ? (
        <Empty>{state.data.length === 0 ? "No containers." : "No matches."}</Empty>
      ) : (
        <div className="space-y-1.5">
          {filtered.map((c) => {
            const isRunning = c.state === "running" || /^Up\s/.test(c.status);
            const result = lastResult[c.id];
            const busyAction = busy[c.id];
            return (
              <div key={c.id} className="bg-black/30 border border-white/5 rounded-lg p-2.5">
                <div className="flex items-center justify-between gap-2">
                  <div className="min-w-0 flex-1">
                    <div className="flex items-center gap-2 flex-wrap">
                      <button
                        onClick={() => onOpenDetails(c)}
                        className="text-[11px] font-mono font-bold text-white truncate hover:text-primary transition-colors"
                        title="Open container details"
                      >
                        {c.names || c.id.slice(0, 12)}
                      </button>
                      <StatusBadge tone={isRunning ? "green" : "zinc"}>{c.state || (isRunning ? "running" : "stopped")}</StatusBadge>
                    </div>
                    <div className="text-[10px] text-zinc-500 font-mono mt-0.5 truncate">{c.image}</div>
                    <div className="text-[10px] text-zinc-500 mt-0.5 truncate">{c.status}</div>
                    {c.ports && <div className="text-[10px] text-primary/70 font-mono mt-0.5 truncate">{c.ports}</div>}
                  </div>
                  <div className="flex items-center gap-1 shrink-0">
                    {!isRunning && (
                      <ActionBtn title="Start" disabled={!!busyAction} tone="green" onClick={() => doAction(c, "start")}>
                        {busyAction === "start" ? <Loader2 size={11} className="animate-spin" /> : <Play size={11} />}
                      </ActionBtn>
                    )}
                    {isRunning && (
                      <>
                        <ActionBtn title="Pause" disabled={!!busyAction} onClick={() => doAction(c, "pause")}>
                          {busyAction === "pause" ? <Loader2 size={11} className="animate-spin" /> : <Pause size={11} />}
                        </ActionBtn>
                        <ActionBtn title="Stop" disabled={!!busyAction} tone="amber" onClick={() => doAction(c, "stop", true)}>
                          {busyAction === "stop" ? <Loader2 size={11} className="animate-spin" /> : <Square size={11} />}
                        </ActionBtn>
                      </>
                    )}
                    <ActionBtn title="Restart" disabled={!!busyAction} tone="primary" onClick={() => doAction(c, "restart", true)}>
                      {busyAction === "restart" ? <Loader2 size={11} className="animate-spin" /> : <RotateCw size={11} />}
                    </ActionBtn>
                    {isRunning && (
                      <ActionBtn title="Open terminal in container" onClick={() => onOpenContainerTerminal(c.names || c.id, false)}>
                        <Terminal size={11} />
                      </ActionBtn>
                    )}
                    <ActionBtn title="Details (logs / inspect / stats)" onClick={() => onOpenDetails(c)}>
                      <Eye size={11} />
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
    </div>
  );
};

const ImagesView = ({ state }: { state: TabState<ImageRow[]> }) => {
  const [showAll, setShowAll] = useState(false);
  const [filter, setFilter] = useState("");
  if (state.loading && !state.data) return <Spinner label="Loading images…" />;
  if (state.error) return <ErrorBanner err={state.error} />;
  if (!state.data) return null;
  const filtered = filter.trim()
    ? state.data.filter(i => `${i.repo}:${i.tag}`.toLowerCase().includes(filter.toLowerCase()))
    : state.data;
  const visible = showAll ? filtered : filtered.slice(0, 30);
  return (
    <div className="space-y-2">
      <div className="flex items-center justify-between gap-2">
        <span className="text-[10px] uppercase tracking-wider text-zinc-500 font-bold">{filtered.length} images</span>
        <FilterInput value={filter} onChange={setFilter} placeholder="repo:tag" />
      </div>
      {filtered.length === 0 ? <Empty>No images.</Empty> : (
        <div className="space-y-1">
          {visible.map((img, i) => (
            <div key={i} className="bg-black/30 border border-white/5 rounded px-2 py-1.5 flex items-center justify-between gap-2">
              <div className="min-w-0">
                <div className="text-[11px] font-mono font-bold text-white truncate">{img.repo}<span className="text-zinc-500">:{img.tag}</span></div>
                <div className="text-[10px] text-zinc-500 truncate">{img.id.slice(0, 12)} · {img.createdSince}</div>
              </div>
              <span className="shrink-0 text-[10px] text-primary/90 font-mono font-bold">{img.size}</span>
            </div>
          ))}
          {filtered.length > 30 && (
            <button onClick={() => setShowAll(s => !s)} className="w-full h-7 rounded-md border border-white/10 text-[10px] font-bold uppercase tracking-wider text-zinc-400 hover:bg-white/5 hover:text-white">
              {showAll ? "Show fewer" : `Show all ${filtered.length}`}
            </button>
          )}
        </div>
      )}
    </div>
  );
};

const VolumesView = ({ state }: { state: TabState<VolumeRow[]> }) => {
  const [showAll, setShowAll] = useState(false);
  const [filter, setFilter] = useState("");
  if (state.loading && !state.data) return <Spinner label="Loading volumes…" />;
  if (state.error) return <ErrorBanner err={state.error} />;
  if (!state.data) return null;
  const filtered = filter.trim()
    ? state.data.filter(v => v.name.toLowerCase().includes(filter.toLowerCase()))
    : state.data;
  const visible = showAll ? filtered : filtered.slice(0, 30);
  return (
    <div className="space-y-2">
      <div className="flex items-center justify-between gap-2">
        <span className="text-[10px] uppercase tracking-wider text-zinc-500 font-bold">{filtered.length} volumes</span>
        <FilterInput value={filter} onChange={setFilter} placeholder="Name" />
      </div>
      {filtered.length === 0 ? <Empty>No volumes.</Empty> : (
        <div className="space-y-1">
          {visible.map((v, i) => (
            <div key={i} className="bg-black/30 border border-white/5 rounded px-2 py-1.5">
              <div className="text-[11px] font-mono font-bold text-white truncate">{v.name}</div>
              <div className="text-[10px] text-zinc-500 font-mono truncate">{v.driver} · {v.mountpoint}</div>
            </div>
          ))}
          {filtered.length > 30 && (
            <button onClick={() => setShowAll(s => !s)} className="w-full h-7 rounded-md border border-white/10 text-[10px] font-bold uppercase tracking-wider text-zinc-400 hover:bg-white/5 hover:text-white">
              {showAll ? "Show fewer" : `Show all ${filtered.length}`}
            </button>
          )}
        </div>
      )}
    </div>
  );
};

const NetworksView = ({ state }: { state: TabState<NetworkRow[]> }) => {
  if (state.loading && !state.data) return <Spinner label="Loading networks…" />;
  if (state.error) return <ErrorBanner err={state.error} />;
  if (!state.data) return null;
  return (
    <div className="space-y-1">
      <span className="text-[10px] uppercase tracking-wider text-zinc-500 font-bold">{state.data.length} networks</span>
      {state.data.length === 0 ? <Empty>No networks.</Empty> : (
        <div className="space-y-1 mt-1">
          {state.data.map((n, i) => (
            <div key={i} className="bg-black/30 border border-white/5 rounded px-2 py-1.5 flex items-center justify-between gap-2">
              <div className="min-w-0">
                <div className="text-[11px] font-mono font-bold text-white truncate">{n.name}</div>
                <div className="text-[10px] text-zinc-500 font-mono truncate">{n.driver} · scope:{n.scope}</div>
              </div>
              <span className="shrink-0 text-[9px] font-mono text-zinc-600">{n.id.slice(0, 12)}</span>
            </div>
          ))}
        </div>
      )}
    </div>
  );
};

const ComposeView = ({
  state, rawFallback, onOpen,
}: {
  state: TabState<ComposeContainer[]>;
  rawFallback: string;
  onOpen: (c: ComposeContainer) => void;
}) => {
  const [filter, setFilter] = useState("");
  if (state.loading && !state.data) return <Spinner label="Reading container compose labels…" />;
  if (state.error) return <ErrorBanner err={state.error} />;
  if (!state.data) return null;

  const filtered = filter.trim()
    ? state.data.filter(c =>
        c.containerName.toLowerCase().includes(filter.toLowerCase()) ||
        c.project.toLowerCase().includes(filter.toLowerCase()) ||
        c.service.toLowerCase().includes(filter.toLowerCase()) ||
        c.configFile.toLowerCase().includes(filter.toLowerCase()))
    : state.data;

  // Group by project so the user sees compose families together. One
  // project usually means one compose file; grouping makes the visual
  // mapping "this file → these containers" obvious.
  const byProject = new Map<string, ComposeContainer[]>();
  for (const c of filtered) {
    if (!byProject.has(c.project)) byProject.set(c.project, []);
    byProject.get(c.project)!.push(c);
  }
  const projects = [...byProject.entries()].sort(([a], [b]) => a.localeCompare(b));

  const hasRaw = rawFallback.trim().length > 0;
  return (
    <div className="space-y-2">
      <div className="flex items-center justify-between gap-2">
        <span className="text-[10px] text-zinc-500">
          {state.data.length} container{state.data.length === 1 ? "" : "s"} started by compose
        </span>
        <FilterInput value={filter} onChange={setFilter} placeholder="Name / project / service" />
      </div>

      {filtered.length === 0 ? (
        <>
          <Empty>{state.data.length === 0 ? "No containers carry compose labels — nothing here was started by docker compose." : "No matches."}</Empty>
          {state.data.length === 0 && hasRaw && (
            <Card title="Raw docker output">
              <pre className="text-[10.5px] font-mono text-zinc-300 whitespace-pre overflow-x-auto select-text bg-black/50 border border-white/5 rounded p-2 max-h-[200px] overflow-y-auto">{rawFallback}</pre>
            </Card>
          )}
        </>
      ) : (
        <div className="space-y-3">
          {projects.map(([project, conts]) => {
            // All containers in a project should share the same compose
            // file (compose enforces this via labels), but just in case we
            // de-dup defensively before rendering.
            const files = Array.from(new Set(conts.map(c => c.configFile).filter(Boolean)));
            return (
              <div key={project} className="bg-black/30 border border-white/5 rounded-lg overflow-hidden">
                <div className="px-3 py-2 border-b border-white/5 bg-white/[0.02]">
                  <div className="flex items-center justify-between gap-2">
                    <span className="text-[11px] font-mono font-bold text-white truncate">{project}</span>
                    <span className="text-[9px] uppercase tracking-wider text-zinc-500">{conts.length} container{conts.length === 1 ? "" : "s"}</span>
                  </div>
                  {files.map((f, i) => (
                    <div key={i} className="text-[10px] font-mono text-zinc-500/80 mt-0.5 truncate select-text" title={f}>{f}</div>
                  ))}
                </div>
                <div className="divide-y divide-white/[0.03]">
                  {conts.map((c, i) => (
                    <button
                      key={i}
                      onClick={() => onOpen(c)}
                      disabled={!c.configFile}
                      title={c.configFile ? "View docker-compose.yml" : "No compose file path on this container's labels"}
                      className="w-full text-left px-3 py-2 hover:bg-primary/5 disabled:opacity-50 disabled:cursor-not-allowed transition-all group flex items-center justify-between gap-2"
                    >
                      <div className="min-w-0 flex-1">
                        <div className="flex items-center gap-2">
                          <span className="text-[11px] font-mono text-white truncate">{c.containerName}</span>
                          {c.service && (
                            <span className="text-[9px] text-zinc-500 font-mono">· svc:{c.service}</span>
                          )}
                          <StatusBadge tone={c.state === "running" ? "green" : "zinc"}>{c.state || "unknown"}</StatusBadge>
                        </div>
                      </div>
                      <div className="flex items-center gap-1.5 shrink-0">
                        <FileText size={11} className="text-zinc-500 group-hover:text-primary" />
                        <ChevronRight size={11} className="text-zinc-500 group-hover:text-primary" />
                      </div>
                    </button>
                  ))}
                </div>
              </div>
            );
          })}
        </div>
      )}
    </div>
  );
};

// Umbrella for images / volumes / networks — three lists that share a single
// sub-tab slot because they're all "static" enumerations the user dips into
// occasionally rather than working with continuously. Each loads lazily on
// first selection so we never round-trip for the two the user doesn't pick.
const ResourcesView = ({
  images, volumes, networks,
  fetchImages, fetchVolumes, fetchNetworks,
}: {
  images: TabState<ImageRow[]>;
  volumes: TabState<VolumeRow[]>;
  networks: TabState<NetworkRow[]>;
  fetchImages: () => void;
  fetchVolumes: () => void;
  fetchNetworks: () => void;
}) => {
  const [kind, setKind] = useState<ResourceKind>("images");

  useEffect(() => {
    if (kind === "images"   && !images.loaded   && !images.loading)   fetchImages();
    if (kind === "volumes"  && !volumes.loaded  && !volumes.loading)  fetchVolumes();
    if (kind === "networks" && !networks.loaded && !networks.loading) fetchNetworks();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [kind]);

  return (
    <div className="space-y-2">
      <div className="flex items-center gap-1.5">
        <KindPill active={kind === "images"}   onClick={() => setKind("images")}   icon={<ImageIcon size={11} />} count={images.data?.length}>Images</KindPill>
        <KindPill active={kind === "volumes"}  onClick={() => setKind("volumes")}  icon={<Database size={11} />}  count={volumes.data?.length}>Volumes</KindPill>
        <KindPill active={kind === "networks"} onClick={() => setKind("networks")} icon={<NetIcon size={11} />}   count={networks.data?.length}>Networks</KindPill>
      </div>
      {kind === "images"   && <ImagesView state={images} />}
      {kind === "volumes"  && <VolumesView state={volumes} />}
      {kind === "networks" && <NetworksView state={networks} />}
    </div>
  );
};

const KindPill = ({
  active, onClick, icon, count, children,
}: {
  active: boolean;
  onClick: () => void;
  icon: React.ReactNode;
  count?: number;
  children: React.ReactNode;
}) => (
  <button
    onClick={onClick}
    className={`h-9 sm:h-7 px-3 sm:px-2.5 rounded-md text-[10.5px] font-bold uppercase tracking-wider transition-all border flex items-center gap-1.5 ${
      active
        ? "bg-primary/15 text-primary border-primary/30"
        : "bg-white/[0.04] text-zinc-400 border-white/10 hover:bg-white/[0.08] hover:text-white"
    }`}
  >
    {icon} <span>{children}</span>
    {count !== undefined && <span className="text-[9.5px] opacity-70">{count}</span>}
  </button>
);

const PruneView = ({ sessionId }: { sessionId: string }) => {
  const [busy, setBusy] = useState<string | null>(null);
  const [result, setResult] = useState<Record<string, { ok: boolean; msg: string; sudo: boolean }>>({});

  const scopes: { key: string; label: string; description: string; warning?: string }[] = [
    { key: "containers", label: "Stopped Containers", description: "Removes containers that are not running. Safe — restartable containers are unaffected." },
    { key: "images",     label: "Dangling Images",    description: "Removes images with no tag and no container referencing them. Tagged images are untouched." },
    { key: "networks",   label: "Unused Networks",    description: "Removes user-created networks no container is attached to. Default bridge/host/none are untouched." },
    { key: "builder",    label: "Build Cache",        description: "Clears Docker BuildKit cache. Future builds re-download / rebuild base layers." },
    { key: "system",     label: "Everything (safe)",  description: "Runs all of the above. Does NOT touch named volumes or tagged images.", warning: "Combines all safe prune scopes in one go." },
  ];

  const run = async (scope: string, label: string) => {
    if (!window.confirm(`Run prune for ${label}?\n\nThis is irreversible.`)) return;
    setBusy(scope);
    setResult(r => ({ ...r, [scope]: undefined as any }));
    try {
      const r = await invoke<DockerActionResult>("ssh_docker_prune", { sessionId, scope });
      setResult(prev => ({
        ...prev,
        [scope]: {
          ok: r.success,
          msg: r.output || (r.success ? "Done" : "Failed"),
          sudo: r.used_sudo,
        },
      }));
    } catch (e: any) {
      setResult(prev => ({ ...prev, [scope]: { ok: false, msg: typeof e === "string" ? e : (e?.message || "failed"), sudo: false } }));
    } finally {
      setBusy(null);
    }
  };

  return (
    <div className="space-y-2">
      <div className="px-3 py-2 rounded-lg bg-amber-500/5 border border-amber-500/20 text-amber-300 text-[10.5px] flex items-start gap-2">
        <ShieldAlert size={14} className="mt-0.5 shrink-0" />
        <div>
          <div className="font-bold uppercase tracking-wider">Safe prune only</div>
          <div className="text-amber-200/80 mt-0.5">Submarine never runs <code className="font-mono px-1">prune -a</code> or <code className="font-mono px-1">--volumes</code>. Named volumes and tagged images are always preserved.</div>
        </div>
      </div>
      {scopes.map(s => {
        const r = result[s.key];
        return (
          <div key={s.key} className="bg-black/30 border border-white/5 rounded-lg p-2.5">
            <div className="flex items-start justify-between gap-2">
              <div className="min-w-0 flex-1">
                <div className="text-[11px] font-bold text-white">{s.label}</div>
                <div className="text-[10px] text-zinc-500 mt-0.5">{s.description}</div>
                {s.warning && <div className="text-[10px] text-amber-400/80 mt-0.5">{s.warning}</div>}
              </div>
              <button
                onClick={() => run(s.key, s.label)}
                disabled={busy !== null}
                className="shrink-0 h-7 px-2.5 rounded-md text-[10px] font-bold uppercase tracking-wider text-rose-300 bg-rose-500/10 border border-rose-500/30 hover:bg-rose-500/20 disabled:opacity-40 disabled:cursor-not-allowed flex items-center gap-1.5 transition-all"
              >
                {busy === s.key ? <Loader2 size={11} className="animate-spin" /> : <Trash2 size={11} />}
                Prune
              </button>
            </div>
            {r && (
              <div className={`mt-1.5 text-[10px] px-2 py-1 rounded font-mono whitespace-pre-wrap break-words ${r.ok ? "bg-emerald-500/10 text-emerald-300 border border-emerald-500/20" : "bg-red-500/10 text-red-300 border border-red-500/20"}`}>
                <span className="font-bold uppercase tracking-wider mr-1 not-italic">{r.ok ? "OK" : "Error"}</span>
                {r.msg}
              </div>
            )}
          </div>
        );
      })}
    </div>
  );
};

// ============== Modals ==============

const ContainerDetailsModal = ({
  sessionId, container, onClose, onOpenContainerTerminal,
}: {
  sessionId: string;
  container: Container;
  onClose: () => void;
  onOpenContainerTerminal: (name: string, useSudo: boolean) => void;
}) => {
  const name = container.names || container.id;
  type DetailTab = "logs" | "inspect" | "stats" | "config";
  const [tab, setTab] = useState<DetailTab>("logs");

  // ----- Logs state (one-shot + live) -----
  const [logs, setLogs] = useState<string>("");
  const [logsTail, setLogsTail] = useState(500);
  const [logsLoading, setLogsLoading] = useState(false);
  const [logsError, setLogsError] = useState<string | null>(null);
  const [live, setLive] = useState(false);
  const liveStreamIdRef = useRef<string | null>(null);
  const logsBoxRef = useRef<HTMLPreElement | null>(null);
  const autoScrollRef = useRef(true);

  const fetchLogs = async (tail: number) => {
    setLogsLoading(true); setLogsError(null);
    try {
      const r = await invoke<DockerTextResult>("ssh_docker_logs", { sessionId, container: name, tail });
      setLogs(r.data || "");
      // Auto-scroll on first load + each load-more / refresh.
      requestAnimationFrame(() => {
        if (logsBoxRef.current && autoScrollRef.current) {
          logsBoxRef.current.scrollTop = logsBoxRef.current.scrollHeight;
        }
      });
    } catch (e: any) {
      setLogsError(typeof e === "string" ? e : (e?.message || "logs failed"));
    } finally { setLogsLoading(false); }
  };

  const startLive = async () => {
    if (live || liveStreamIdRef.current) return;
    const sid = `${sessionId}-${name}-${Date.now()}`;
    liveStreamIdRef.current = sid;
    setLive(true);
    // Pre-load current tail so the viewer isn't empty until something new
    // is written. The follow stream will append to whatever's there.
    if (!logs) await fetchLogs(logsTail);
    try {
      const unlistenLine = await listen<string>(`docker-logs-${sid}`, (e) => {
        setLogs(prev => {
          const next = prev + e.payload;
          // Trim from the front so memory doesn't explode in a long live
          // tail. 1 MB of text is plenty of scroll-back for triage.
          return next.length > 1_000_000 ? next.slice(next.length - 800_000) : next;
        });
      });
      const unlistenClose = await listen(`docker-logs-closed-${sid}`, () => {
        setLive(false);
        liveStreamIdRef.current = null;
      });
      await invoke("ssh_docker_logs_start", { sessionId, streamId: sid, container: name, tail: 0 });
      // Clean up listeners when live ends or modal closes.
      const cleanup = () => { unlistenLine(); unlistenClose(); };
      (liveStreamIdRef as any).cleanup = cleanup;
    } catch (e: any) {
      setLogsError(typeof e === "string" ? e : (e?.message || "live start failed"));
      setLive(false);
      liveStreamIdRef.current = null;
    }
  };
  const stopLive = async () => {
    const sid = liveStreamIdRef.current;
    if (!sid) { setLive(false); return; }
    try { await invoke("ssh_docker_logs_stop", { streamId: sid }); } catch {}
    if ((liveStreamIdRef as any).cleanup) {
      try { (liveStreamIdRef as any).cleanup(); } catch {}
      delete (liveStreamIdRef as any).cleanup;
    }
    liveStreamIdRef.current = null;
    setLive(false);
  };

  // Auto-scroll only if user is already at bottom — they may have scrolled up
  // to inspect a past line, in which case yanking them down each time a new
  // line arrives would be hostile.
  const onLogsScroll = () => {
    const el = logsBoxRef.current;
    if (!el) return;
    autoScrollRef.current = el.scrollTop + el.clientHeight >= el.scrollHeight - 8;
  };

  useEffect(() => {
    if (live && logsBoxRef.current && autoScrollRef.current) {
      logsBoxRef.current.scrollTop = logsBoxRef.current.scrollHeight;
    }
  }, [logs, live]);

  // ----- Inspect / Stats / Config (lazy on tab swap) -----
  const [inspect, setInspect] = useState<{ loading: boolean; error: string | null; data: any | null }>({ loading: false, error: null, data: null });
  const [oneStats, setOneStats] = useState<{ loading: boolean; error: string | null; data: ContainerStats | null }>({ loading: false, error: null, data: null });

  const fetchInspect = async () => {
    setInspect(s => ({ ...s, loading: true, error: null }));
    try {
      const r = await invoke<DockerTextResult>("ssh_docker_inspect", { sessionId, kind: "container", name });
      const parsed = JSON.parse(r.data);
      setInspect({ loading: false, error: null, data: Array.isArray(parsed) ? parsed[0] : parsed });
    } catch (e: any) {
      setInspect({ loading: false, error: typeof e === "string" ? e : (e?.message || "inspect failed"), data: null });
    }
  };

  const fetchOneStats = async () => {
    setOneStats(s => ({ ...s, loading: true, error: null }));
    try {
      const r = await invoke<DockerTextResult>("ssh_docker_stats", { sessionId });
      const rows = parseJsonl<ContainerStats>(r.data, o => ({
        id: o.ID || "", name: o.Name || "",
        cpuPerc: o.CPUPerc || "", memUsage: o.MemUsage || "", memPerc: o.MemPerc || "",
        netIO: o.NetIO || "", blockIO: o.BlockIO || "", pids: o.PIDs || "",
      }));
      const found = rows.find(s => s.name === name);
      setOneStats({ loading: false, error: null, data: found || null });
    } catch (e: any) {
      setOneStats({ loading: false, error: typeof e === "string" ? e : (e?.message || "stats failed"), data: null });
    }
  };

  useEffect(() => {
    if (tab === "logs" && logs === "" && !logsLoading) fetchLogs(logsTail);
    if (tab === "inspect" && !inspect.data && !inspect.loading) fetchInspect();
    if (tab === "stats" && !oneStats.data && !oneStats.loading) fetchOneStats();
    if (tab === "config" && !inspect.data && !inspect.loading) fetchInspect(); // config view reads from inspect
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [tab]);

  // Stop live + remove listeners when the modal unmounts.
  useEffect(() => {
    return () => {
      const sid = liveStreamIdRef.current;
      if (sid) { invoke("ssh_docker_logs_stop", { streamId: sid }).catch(() => {}); }
      if ((liveStreamIdRef as any).cleanup) { try { (liveStreamIdRef as any).cleanup(); } catch {} }
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  return (
    <ModalShell title={`Container · ${name}`} onClose={onClose} actions={
      <button
        onClick={() => onOpenContainerTerminal(name, false)}
        className="h-7 px-2.5 rounded-md text-[10px] font-bold uppercase tracking-wider text-sky-200 bg-sky-500/15 border border-sky-500/30 hover:bg-sky-500/25 flex items-center gap-1.5 transition-all"
      >
        <Terminal size={11} /> Open Terminal
      </button>
    }>
      <div className="flex items-center gap-1.5 px-3 pt-2 pb-1 bg-black/20 border-b border-white/5 shrink-0 overflow-x-auto no-scrollbar">
        <SubBtn active={tab === "logs"}    onClick={() => setTab("logs")}    icon={<FileText size={11} />}>Logs</SubBtn>
        <SubBtn active={tab === "stats"}   onClick={() => setTab("stats")}   icon={<Activity size={11} />}>Stats</SubBtn>
        <SubBtn active={tab === "config"}  onClick={() => setTab("config")}  icon={<Box size={11} />}>Env · Mounts · Ports</SubBtn>
        <SubBtn active={tab === "inspect"} onClick={() => setTab("inspect")} icon={<Eye size={11} />}>Inspect</SubBtn>
      </div>

      <div className="flex-1 overflow-y-auto custom-scrollbar p-3 select-text">
        {tab === "logs" && (
          <div className="space-y-2 h-full flex flex-col">
            <div className="flex items-center gap-2 flex-wrap shrink-0">
              <select
                value={logsTail}
                onChange={(e) => setLogsTail(parseInt(e.target.value, 10))}
                className="h-7 px-2 bg-black/40 border border-white/10 rounded text-[10px] text-zinc-200 font-mono focus:border-primary/50 outline-none"
              >
                <option value={500}>Tail 500</option>
                <option value={2000}>Tail 2 000</option>
                <option value={5000}>Tail 5 000</option>
                <option value={20000}>Tail 20 000</option>
                <option value={50000}>Tail 50 000</option>
              </select>
              <button
                onClick={() => fetchLogs(logsTail)}
                disabled={logsLoading || live}
                className="h-7 px-2 rounded-md text-[10px] font-bold uppercase tracking-wider text-zinc-300 bg-white/[0.04] border border-white/10 hover:bg-white/[0.08] hover:text-white disabled:opacity-40 disabled:cursor-not-allowed flex items-center gap-1.5"
              >
                {logsLoading ? <Loader2 size={11} className="animate-spin" /> : <RefreshCw size={11} />} Reload
              </button>
              <button
                onClick={live ? stopLive : startLive}
                className={`h-7 px-2 rounded-md text-[10px] font-bold uppercase tracking-wider flex items-center gap-1.5 transition-all ${live ? "bg-rose-500/15 text-rose-300 border border-rose-500/30 hover:bg-rose-500/25" : "bg-emerald-500/15 text-emerald-300 border border-emerald-500/30 hover:bg-emerald-500/25"}`}
              >
                {live ? <><Square size={11} /> Stop live</> : <><Play size={11} /> Go live</>}
              </button>
              {live && <span className="text-[10px] text-emerald-400 font-mono animate-pulse">● streaming</span>}
              <button
                onClick={() => { navigator.clipboard.writeText(logs).catch(() => {}); }}
                className="ml-auto h-7 px-2 rounded-md text-[10px] font-bold uppercase tracking-wider text-zinc-400 hover:text-white hover:bg-white/5 flex items-center gap-1.5"
              >
                <Copy size={11} /> Copy
              </button>
            </div>
            {logsError && <ErrorBanner err={logsError} />}
            <pre
              ref={logsBoxRef}
              onScroll={onLogsScroll}
              className="flex-1 min-h-[280px] bg-black/50 border border-white/5 rounded-lg p-2 text-[10.5px] font-mono text-zinc-200 whitespace-pre overflow-auto select-text"
            >
              {logs || (logsLoading ? "Loading…" : "(no log output)")}
            </pre>
          </div>
        )}

        {tab === "stats" && (
          <div className="space-y-2">
            {oneStats.loading && <Spinner label="Reading stats…" />}
            {oneStats.error && <ErrorBanner err={oneStats.error} />}
            {oneStats.data && (
              <div className="grid grid-cols-1 [@media(min-width:560px)]:grid-cols-2 gap-2">
                <StatTile label="CPU" value={oneStats.data.cpuPerc} />
                <StatTile label="Memory" value={`${oneStats.data.memPerc}`} sub={oneStats.data.memUsage} />
                <StatTile label="Net I/O" value={oneStats.data.netIO} />
                <StatTile label="Block I/O" value={oneStats.data.blockIO} />
                <StatTile label="PIDs" value={oneStats.data.pids} />
              </div>
            )}
            {!oneStats.loading && !oneStats.data && !oneStats.error && (
              <Empty>Stats unavailable. Container may be stopped.</Empty>
            )}
            <button
              onClick={fetchOneStats}
              disabled={oneStats.loading}
              className="h-7 px-2.5 rounded-md text-[10px] font-bold uppercase tracking-wider text-zinc-300 bg-white/[0.04] border border-white/10 hover:bg-white/[0.08] hover:text-white disabled:opacity-40 flex items-center gap-1.5"
            >
              {oneStats.loading ? <Loader2 size={11} className="animate-spin" /> : <RefreshCw size={11} />} Refresh
            </button>
          </div>
        )}

        {tab === "config" && (
          <div className="space-y-3">
            {inspect.loading && <Spinner label="Reading config…" />}
            {inspect.error && <ErrorBanner err={inspect.error} />}
            {inspect.data && <ConfigView data={inspect.data} />}
          </div>
        )}

        {tab === "inspect" && (
          <div className="space-y-2 h-full flex flex-col">
            <div className="shrink-0 flex items-center gap-2">
              <button
                onClick={fetchInspect}
                disabled={inspect.loading}
                className="h-7 px-2 rounded-md text-[10px] font-bold uppercase tracking-wider text-zinc-300 bg-white/[0.04] border border-white/10 hover:bg-white/[0.08] hover:text-white disabled:opacity-40 flex items-center gap-1.5"
              >
                {inspect.loading ? <Loader2 size={11} className="animate-spin" /> : <RefreshCw size={11} />} Refresh
              </button>
              <button
                onClick={() => { if (inspect.data) navigator.clipboard.writeText(JSON.stringify(inspect.data, null, 2)).catch(() => {}); }}
                className="ml-auto h-7 px-2 rounded-md text-[10px] font-bold uppercase tracking-wider text-zinc-400 hover:text-white hover:bg-white/5 flex items-center gap-1.5"
              >
                <Copy size={11} /> Copy JSON
              </button>
            </div>
            {inspect.error && <ErrorBanner err={inspect.error} />}
            <pre className="flex-1 min-h-[300px] bg-black/50 border border-white/5 rounded-lg p-2 text-[10.5px] font-mono text-zinc-200 whitespace-pre overflow-auto select-text">
              {inspect.data ? JSON.stringify(inspect.data, null, 2) : (inspect.loading ? "Loading…" : "(no data)")}
            </pre>
          </div>
        )}
      </div>
    </ModalShell>
  );
};

const ConfigView = ({ data }: { data: any }) => {
  // Pull common, useful fields out of the inspect JSON so the user doesn't
  // have to scroll a 10 KB blob to read their env or port bindings.
  const config = data?.Config || {};
  const host = data?.HostConfig || {};
  const netSettings = data?.NetworkSettings || {};
  const env: string[] = config.Env || [];
  const mounts: any[] = data?.Mounts || [];
  const portBindings: Record<string, any[] | null> = host.PortBindings || {};
  const exposed: Record<string, any> = config.ExposedPorts || {};
  const networks: Record<string, any> = netSettings.Networks || {};

  return (
    <>
      <Card title={`Environment (${env.length})`}>
        {env.length === 0 ? <Empty>No env vars set.</Empty> : (
          <div className="space-y-0.5">
            {env.map((kv, i) => {
              const eq = kv.indexOf("=");
              const k = eq > 0 ? kv.slice(0, eq) : kv;
              const v = eq > 0 ? kv.slice(eq + 1) : "";
              return (
                <div key={i} className="text-[11px] font-mono flex gap-2">
                  <span className="text-primary/80 shrink-0">{k}</span>
                  <span className="text-zinc-500">=</span>
                  <span className="text-zinc-300 break-all min-w-0">{v}</span>
                </div>
              );
            })}
          </div>
        )}
      </Card>

      <Card title={`Mounts (${mounts.length})`}>
        {mounts.length === 0 ? <Empty>No mounts.</Empty> : (
          <div className="space-y-1">
            {mounts.map((m, i) => (
              <div key={i} className="bg-black/30 border border-white/5 rounded px-2 py-1 text-[10.5px] font-mono">
                <div className="text-zinc-300 truncate"><span className="text-zinc-500">{m.Type}:</span> {m.Source || m.Name}</div>
                <div className="text-zinc-400 truncate">→ {m.Destination} <span className="text-zinc-600">{m.Mode || (m.RW === false ? "ro" : "rw")}</span></div>
              </div>
            ))}
          </div>
        )}
      </Card>

      <Card title="Ports">
        {Object.keys(portBindings).length === 0 && Object.keys(exposed).length === 0 ? (
          <Empty>No ports.</Empty>
        ) : (
          <div className="space-y-0.5">
            {Object.entries(portBindings).map(([proto, bindings]) => (
              <div key={proto} className="text-[11px] font-mono">
                <span className="text-zinc-200">{proto}</span>
                {bindings && bindings.length > 0 && (
                  <span className="text-zinc-500"> → {bindings.map(b => `${b.HostIp || "0.0.0.0"}:${b.HostPort}`).join(", ")}</span>
                )}
              </div>
            ))}
            {Object.keys(exposed).filter(p => !(p in portBindings)).map(p => (
              <div key={p} className="text-[11px] font-mono text-zinc-500">{p} <span className="text-zinc-600 italic">(exposed only)</span></div>
            ))}
          </div>
        )}
      </Card>

      <Card title={`Networks (${Object.keys(networks).length})`}>
        {Object.keys(networks).length === 0 ? <Empty>Not connected to any network.</Empty> : (
          <div className="space-y-1">
            {Object.entries(networks).map(([name, n]: any) => (
              <div key={name} className="bg-black/30 border border-white/5 rounded px-2 py-1 text-[10.5px] font-mono">
                <div className="text-zinc-200">{name}</div>
                <div className="text-zinc-500">{n.IPAddress || "—"} · gateway {n.Gateway || "—"}</div>
              </div>
            ))}
          </div>
        )}
      </Card>
    </>
  );
};

// Per-container compose viewer. Simpler than the project-level modal it
// replaced: just the file path + the YAML body. No project actions, no
// service listing — those would be confusing here because the user came
// in from a specific container, not from "the whole project". The
// existing container-level Start/Stop/Restart in the Containers tab
// covers the action needs they had in mind.
const ComposeViewerModal = ({
  sessionId, container, onClose,
}: {
  sessionId: string;
  container: ComposeContainer;
  onClose: () => void;
}) => {
  const filePath = container.configFile;
  const [yaml, setYaml] = useState<{ loading: boolean; error: string | null; data: string | null }>({ loading: false, error: null, data: null });
  const [copied, setCopied] = useState(false);

  const loadYaml = async () => {
    if (!filePath) return;
    setYaml(s => ({ ...s, loading: true, error: null }));
    try {
      const r = await invoke<DockerTextResult>("ssh_docker_compose_view", { sessionId, composeFile: filePath });
      if (!r.success) throw new Error(r.data || "compose view failed");
      setYaml({ loading: false, error: null, data: r.data });
    } catch (e: any) {
      setYaml({ loading: false, error: typeof e === "string" ? e : (e?.message || "view failed"), data: null });
    }
  };

  useEffect(() => { loadYaml(); /* eslint-disable-next-line */ }, []);

  const copyPath = () => {
    navigator.clipboard.writeText(filePath).then(() => {
      setCopied(true);
      setTimeout(() => setCopied(false), 1500);
    }).catch(() => {});
  };

  return (
    <ModalShell title={`compose.yaml · ${container.containerName}`} onClose={onClose}>
      <div className="px-3 py-2 border-b border-white/5 bg-black/20 shrink-0 space-y-1">
        <div className="flex items-center gap-2">
          <span className="text-[9px] uppercase tracking-wider text-zinc-500 font-bold shrink-0">Container</span>
          <code className="text-[10.5px] font-mono text-zinc-200 truncate select-text">{container.containerName}</code>
          <span className="text-[9px] text-zinc-500 ml-2 shrink-0">project: <span className="text-zinc-300 font-mono">{container.project}</span></span>
          {container.service && <span className="text-[9px] text-zinc-500 ml-1 shrink-0">· service: <span className="text-zinc-300 font-mono">{container.service}</span></span>}
        </div>
        <div className="flex items-center gap-2">
          <span className="text-[9px] uppercase tracking-wider text-zinc-500 font-bold shrink-0">File</span>
          <code className="text-[10.5px] font-mono text-zinc-200 truncate select-text flex-1 min-w-0">{filePath || "(no compose file label)"}</code>
          {filePath && (
            <button onClick={copyPath} title="Copy path" className="shrink-0 p-1 rounded text-zinc-500 hover:text-white hover:bg-white/5">
              {copied ? <Check size={11} className="text-emerald-400" /> : <Copy size={11} />}
            </button>
          )}
        </div>
      </div>

      <div className="flex-1 overflow-y-auto custom-scrollbar p-3 select-text">
        {!filePath ? (
          <Empty>No compose file path on this container's labels.</Empty>
        ) : (
          <Card title="compose.yaml" actions={
            yaml.data && (
              <button onClick={() => { navigator.clipboard.writeText(yaml.data!).catch(() => {}); }}
                className="h-6 px-2 rounded text-[10px] font-bold uppercase tracking-wider text-zinc-400 hover:text-white hover:bg-white/5 flex items-center gap-1">
                <Copy size={10} /> Copy
              </button>
            )
          }>
            <div className="px-2 py-1 mb-2 rounded bg-amber-500/5 border border-amber-500/20 text-amber-300 text-[10px] flex items-start gap-1.5">
              <AlertTriangle size={11} className="mt-0.5 shrink-0" />
              <span>This file may contain secrets (DB passwords, API keys). View-only — Submarine never edits compose files.</span>
            </div>
            {yaml.loading && <Spinner label="Reading compose file…" />}
            {yaml.error && <ErrorBanner err={yaml.error} />}
            {yaml.data && (
              <pre className="bg-black/50 border border-white/5 rounded-lg p-2 text-[10.5px] font-mono text-zinc-200 whitespace-pre overflow-auto max-h-[60vh] select-text">
                {yaml.data}
              </pre>
            )}
          </Card>
        )}
      </div>
    </ModalShell>
  );
};

// ============== Reusable bits ==============

const SubBtn = ({ active, onClick, icon, children }: { active: boolean; onClick: () => void; icon?: React.ReactNode; children: React.ReactNode }) => (
  <button onClick={onClick} className={`${subTabBase} ${active ? subTabActive : subTabIdle} snap-start`}>
    {icon} {children}
  </button>
);

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

const CountPill = ({ active, onClick, tone, count, children }: { active: boolean; onClick: () => void; tone: "green" | "zinc" | "primary"; count: number; children: React.ReactNode }) => {
  const baseTone =
    tone === "green" ? (active ? "bg-emerald-500/15 text-emerald-300 border-emerald-500/30" : "bg-white/[0.04] text-zinc-400 border-white/10")
    : tone === "primary" ? (active ? "bg-primary/15 text-primary border-primary/30" : "bg-white/[0.04] text-zinc-400 border-white/10")
    : (active ? "bg-zinc-500/20 text-zinc-200 border-zinc-500/30" : "bg-white/[0.04] text-zinc-400 border-white/10");
  return (
    <button onClick={onClick} className={`h-9 sm:h-7 px-3 sm:px-2 rounded-md text-[10.5px] font-bold uppercase tracking-wider transition-all border flex items-center gap-1.5 ${baseTone} hover:brightness-110`}>
      <span>{children}</span>
      <span className="text-[9.5px] opacity-70">{count}</span>
    </button>
  );
};

const ActionBtn = ({ onClick, disabled, title, tone, children }: { onClick: () => void; disabled?: boolean; title: string; tone?: "amber" | "primary" | "green"; children: React.ReactNode }) => {
  const cls = tone === "amber" ? "text-amber-300 hover:bg-amber-500/10 border-amber-500/20"
    : tone === "primary" ? "text-primary hover:bg-primary/10 border-primary/20"
    : tone === "green" ? "text-emerald-300 hover:bg-emerald-500/10 border-emerald-500/20"
    : "text-zinc-300 hover:bg-white/10 border-white/10";
  return (
    // h-8 w-8 (32 px) on phones — under iOS's 44 pt sweet spot but the
    // best we can do here without each container row collapsing into 2
    // lines on a 360 px viewport. Desktop shrinks to h-7 since pointer
    // accuracy is finer than a thumb tip.
    <button onClick={onClick} disabled={disabled} title={title}
      className={`h-8 w-8 sm:h-7 sm:w-7 rounded-md border bg-white/[0.04] flex items-center justify-center transition-all disabled:opacity-40 disabled:cursor-not-allowed ${cls}`}>
      {children}
    </button>
  );
};

const ProjectActionBtn = ({ busy, onClick, tone, children }: { busy: boolean; onClick: () => void; tone: "green" | "primary" | "zinc" | "amber"; children: React.ReactNode }) => {
  const cls = tone === "green" ? "text-emerald-300 bg-emerald-500/10 border-emerald-500/30 hover:bg-emerald-500/20"
    : tone === "primary" ? "text-primary bg-primary/10 border-primary/30 hover:bg-primary/20"
    : tone === "amber" ? "text-amber-300 bg-amber-500/10 border-amber-500/30 hover:bg-amber-500/20"
    : "text-zinc-300 bg-white/[0.04] border-white/10 hover:bg-white/[0.08]";
  return (
    <button disabled={busy} onClick={onClick}
      className={`h-9 sm:h-7 px-3 sm:px-2.5 rounded-md text-[10.5px] font-bold uppercase tracking-wider border flex items-center gap-1.5 disabled:opacity-40 ${cls}`}>
      {busy && <Loader2 size={11} className="animate-spin" />}
      {children}
    </button>
  );
};

const StatusBadge = ({ tone, children }: { tone: "green" | "amber" | "red" | "zinc"; children: React.ReactNode }) => {
  const cls = tone === "green" ? "bg-emerald-500/15 text-emerald-300 border-emerald-500/30"
    : tone === "amber" ? "bg-amber-500/15 text-amber-300 border-amber-500/30"
    : tone === "red"   ? "bg-red-500/15 text-red-300 border-red-500/30"
    : "bg-zinc-500/10 text-zinc-400 border-zinc-500/20";
  return <span className={`shrink-0 text-[9px] px-1.5 py-0.5 rounded font-bold uppercase tracking-wider border ${cls}`}>{children}</span>;
};

const Card = ({ title, actions, children }: { title: string; actions?: React.ReactNode; children: React.ReactNode }) => (
  <section className="bg-[#121215] border border-white/5 rounded-xl overflow-hidden">
    <div className="px-3 py-1.5 border-b border-white/5 bg-white/[0.02] flex items-center gap-2">
      <span className="text-[10px] font-bold uppercase tracking-wider text-zinc-400 select-text">{title}</span>
      {actions && <div className="ml-auto">{actions}</div>}
    </div>
    <div className="p-3 select-text">{children}</div>
  </section>
);

const StatTile = ({ label, value, sub }: { label: string; value: string; sub?: string }) => (
  <div className="bg-black/30 border border-white/5 rounded-lg p-2">
    <div className="text-[9px] font-bold uppercase tracking-wider text-zinc-500">{label}</div>
    <div className="text-[14px] font-mono font-bold text-white mt-0.5 select-text">{value || "—"}</div>
    {sub && <div className="text-[10px] text-zinc-500 font-mono mt-0.5 select-text">{sub}</div>}
  </div>
);

const Spinner = ({ label }: { label: string }) => (
  <div className="flex items-center gap-2 text-zinc-400 text-[11px]">
    <Loader2 size={14} className="animate-spin" /> {label}
  </div>
);

const ErrorBanner = ({ err }: { err: string }) => {
  const e = describeError(err);
  return (
    <div className="flex items-start gap-2 px-3 py-2 rounded-lg bg-red-500/5 border border-red-500/20 text-red-300 text-[11px]">
      <AlertCircle size={14} className="mt-0.5 shrink-0" />
      <div className="min-w-0">
        <div className="font-bold uppercase tracking-wider">{e.title}</div>
        {e.hint && <div className="text-red-200/80 mt-0.5">{e.hint}</div>}
        <div className="opacity-70 mt-0.5 font-mono break-all">{err}</div>
      </div>
    </div>
  );
};

const Empty = ({ children }: { children: React.ReactNode }) => (
  <div className="text-[11px] text-zinc-500 italic">{children}</div>
);

const ModalShell = ({ title, onClose, actions, children }: { title: string; onClose: () => void; actions?: React.ReactNode; children: React.ReactNode }) => (
  // On phones we go full-bleed: no outer padding, no rounded corners, no
  // border — the modal IS the screen. From sm: up we revert to a centered
  // card with the soft-edge look. Saves the ~24 px of viewport that a
  // floating card eats on a 360 px screen.
  <div className="fixed inset-0 z-[9999] bg-black/60 backdrop-blur-sm flex sm:items-center sm:justify-center sm:p-3 animate-in fade-in" onClick={onClose}>
    <div onClick={(e) => e.stopPropagation()} className="bg-[#0c0c0e] sm:border sm:border-white/10 sm:rounded-xl w-full sm:max-w-3xl h-full sm:h-[80vh] flex flex-col overflow-hidden shadow-2xl">
      <div className="shrink-0 h-11 sm:h-10 px-3 flex items-center justify-between border-b border-white/5 bg-white/[0.02]">
        <span className="text-[11px] font-bold uppercase tracking-wider text-zinc-300 truncate select-text">{title}</span>
        <div className="flex items-center gap-2 shrink-0">
          {actions}
          {/* h-9 w-9 = 36 px touch target on phone, vs the old icon
              which was a 14 px icon with no padding zone — easy to miss
              with a thumb. */}
          <button onClick={onClose} className="h-9 w-9 sm:h-7 sm:w-7 flex items-center justify-center rounded-md text-zinc-500 hover:text-white hover:bg-white/5">
            <X size={16} />
          </button>
        </div>
      </div>
      {children}
    </div>
  </div>
);

export default DockerTab;
