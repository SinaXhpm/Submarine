// The Agents page (desktop): AI agents on this computer ask here for access to
// a server, the user answers, and every operation of a grant shows as a card.
// Backend: src-tauri/src/gateway/hub.rs. Spec: §5 of the agent gateway spec.
import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { Bot, Check, Loader2, Plug, Server, ShieldCheck, Unplug, X } from "lucide-react";
import { ToolCard, LIVE_CAP, type LiveCall } from "../ai/ToolCard";
import { ExactText } from "../ai/ExactText";
import { errText } from "../../util/aiApi";
import { isFtp } from "../../util/serverProtocol";
import {
  gateway,
  DURATIONS,
  MODES,
  OPS,
  isCommandOp,
  isFileOp,
  minutesText,
  programName,
  type AgentConnection,
  type AgentEventRow,
  type AgentRequest,
  type GatewayCallEvent,
  type GatewayOp,
  type GatewayOverview,
} from "../../util/gatewayApi";

interface ServerItem {
  uuid?: string;
  name: string;
  protocol?: string;
}

interface Props {
  active: boolean;
  /** The saved servers: a request's server is picked from the SSH and FTP ones. */
  servers: ServerItem[];
  /** The connected SSH or FTP session of each saved server, by uuid. */
  openSessions: Record<string, string>;
  serverNames: Record<string, string>;
  /** Opens a saved server in a session tab: an agent works through it. */
  onOpenServer: (uuid: string) => void;
  onOpenSettings: () => void;
  /** How many requests and operations wait, and whether the gateway is on: for the rail. */
  onState?: (waiting: number, enabled: boolean) => void;
}

interface CallItem {
  call: LiveCall;
  grant: string;
  connection: number;
  serverUuid: string;
  live: boolean;
}

/** Finished operations kept on the page. */
const KEEP_DONE = 20;

const timeText = (secs: number) => new Date(secs * 1000).toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" });

/** The request card's buttons don't answer for a moment after it appears, nor
 *  to the second click of a double-click: it may come up under the pointer. */
const useFreshGuard = () => {
  const shown = useRef(performance.now());
  return (e: React.MouseEvent) => e.detail > 1 || (e.isTrusted && performance.now() - shown.current < 400);
};

const agentName = (c: AgentConnection | undefined) =>
  c?.name ? `${c.name}${c.version ? ` ${c.version}` : ""}` : c?.chain[0]?.program ? programName(c.chain[0].program) : "An agent";

export const AgentsPanel = ({ active, servers, openSessions, serverNames, onOpenServer, onOpenSettings, onState }: Props) => {
  const [ov, setOv] = useState<GatewayOverview | null>(null);
  const [events, setEvents] = useState<AgentEventRow[]>([]);
  const [calls, setCalls] = useState<Map<string, CallItem>>(new Map());
  const [error, setError] = useState<string | null>(null);

  const load = useCallback(async () => {
    try {
      const o = await gateway.overview();
      setOv(o);
      setError(null);
      if (o.profile_open) setEvents(await gateway.events().catch(() => []));
      else setEvents([]);
      // Cards that wait show even when their first event came before the page did.
      setCalls((prev) => {
        const next = new Map(prev);
        for (const c of o.cards) {
          if (next.has(c.call_id)) continue;
          next.set(c.call_id, {
            grant: c.grant,
            connection: c.connection,
            serverUuid: c.server_uuid,
            live: true,
            call: { id: c.call_id, tool: c.tool, input: c.input, needs: "approval", reason: c.reason, red: c.red, path: c.path, preview: c.preview, how: null, command: null, out: "", err: "", started: null, record: null },
          });
        }
        return next;
      });
    } catch (e) {
      setError(errText(e));
    }
  }, []);

  useEffect(() => {
    void load();
    const offs = [gateway.onChanged(() => void load()), gateway.onCall((e) => setCalls((prev) => applyCall(prev, e)))];
    return () => offs.forEach((p) => void p.then((off) => off()));
  }, [load]);

  // Time left on a grant counts down on the page too.
  const hasGrants = (ov?.grants.length ?? 0) > 0;
  useEffect(() => {
    if (!hasGrants) return;
    const t = setInterval(() => void load(), 30_000);
    return () => clearInterval(t);
  }, [hasGrants, load]);

  const waitingCards = useMemo(() => [...calls.values()].filter((c) => c.live && c.call.needs === "approval" && !c.call.how && !c.call.record), [calls]);
  const waiting = (ov?.requests.length ?? 0) + waitingCards.length;
  useEffect(() => {
    onState?.(waiting, !!ov?.enabled);
  }, [waiting, ov?.enabled, onState]);

  const conns = useMemo(() => new Map((ov?.connections ?? []).map((c) => [c.id, c])), [ov]);
  const callList = useMemo(() => [...calls.values()].reverse(), [calls]);

  // Rendered while hidden too (the parent hides it): a request card keeps what
  // the user chose while they open its server in a tab and come back.
  void active;

  if (!ov) {
    return (
      <div className="flex-1 flex items-center justify-center text-zinc-500 text-sm gap-2">
        {error ? <span className="text-red-300">{error}</span> : <><Loader2 size={14} className="animate-spin" /> Loading</>}
      </div>
    );
  }

  return (
    <div className="flex-1 min-h-0 overflow-y-auto">
      <div className="max-w-3xl mx-auto p-4 sm:p-6 space-y-6">
        <div className="flex items-center gap-2">
          <Bot size={18} className="text-primary" />
          <h1 className="text-sm font-black uppercase tracking-wider text-zinc-300 flex-1">AI agents</h1>
          <span className={`text-[11px] px-2 py-0.5 rounded-full border ${ov.listening ? "border-emerald-500/30 text-emerald-300" : "border-white/10 text-zinc-500"}`}>
            {ov.listening ? "Listening" : ov.enabled ? "Not listening" : "Off"}
          </span>
        </div>

        {(!ov.enabled || ov.error || !ov.profile_open) && (
          <div className="bg-[#121215] border border-white/5 rounded-2xl p-4 text-[12px] text-zinc-400 leading-relaxed space-y-2">
            {!ov.enabled && <p>Agents can't reach Submarine: the switch in Settings → AI agents is off.</p>}
            {ov.enabled && !ov.profile_open && <p>Open a profile: agents can only ask while one is open.</p>}
            {ov.error && <p className="text-amber-300">{ov.error}</p>}
            <button type="button" onClick={onOpenSettings} className="h-8 px-3 rounded-lg text-[11px] font-bold uppercase border border-white/10 text-zinc-300 hover:bg-white/5">
              Open Settings
            </button>
          </div>
        )}
        {error && <div className="text-[12px] text-red-300">{error}</div>}

        <Section title="Waiting" count={waiting}>
          {waiting === 0 && <Empty>Nothing waits for you.</Empty>}
          {ov.requests.map((r) => (
            <RequestCard
              key={r.id}
              req={r}
              conn={conns.get(r.connection)}
              offered={ov.offered}
              longest={ov.longest_minutes}
              servers={servers}
              openSessions={openSessions}
              onOpenServer={onOpenServer}
              onError={setError}
            />
          ))}
          {waitingCards.map((c) => (
            <CallBlock key={c.call.id} item={c} conn={conns.get(c.connection)} serverNames={serverNames} />
          ))}
        </Section>

        <Section title="Connected" count={ov.connections.length}>
          {ov.connections.length === 0 && <Empty>No agent is connected.</Empty>}
          {ov.connections.map((c) => (
            <ConnectionBlock key={c.id} conn={c} grants={ov.grants.filter((g) => g.connection === c.id)} serverNames={serverNames} onError={setError} />
          ))}
        </Section>

        {callList.some((c) => !(c.live && c.call.needs === "approval" && !c.call.how && !c.call.record)) && (
          <Section title="Operations">
            {callList
              .filter((c) => !(c.live && c.call.needs === "approval" && !c.call.how && !c.call.record))
              .map((c) => (
                <CallBlock key={c.call.id} item={c} conn={conns.get(c.connection)} serverNames={serverNames} />
              ))}
          </Section>
        )}

        <Section title="Activity">
          {events.length === 0 && <Empty>Nothing yet.</Empty>}
          {events.length > 0 && (
            <ul className="divide-y divide-white/5 bg-[#121215] border border-white/5 rounded-2xl">
              {events.map((e) => (
                <li key={e.id} className="px-4 py-2 text-[12px] flex gap-3 items-baseline">
                  <span className="text-zinc-500 tabular-nums shrink-0">{timeText(e.at)}</span>
                  <span className="text-zinc-300 shrink-0 w-24">{e.kind}</span>
                  <span className="text-zinc-400 min-w-0 flex-1 break-words">
                    {e.agent}
                    {e.server_uuid && <> · {serverNames[e.server_uuid] ?? "a removed server"}</>}
                    {e.detail && <span className="text-zinc-500"> · {e.detail}</span>}
                  </span>
                </li>
              ))}
            </ul>
          )}
        </Section>
      </div>
    </div>
  );
};

function applyCall(prev: Map<string, CallItem>, e: GatewayCallEvent): Map<string, CallItem> {
  const ev = e.event;
  if (!("call_id" in ev)) return prev;
  const next = new Map(prev);
  const old = next.get(ev.call_id);
  const base: CallItem = old ?? {
    grant: e.grant,
    connection: e.connection,
    serverUuid: e.server_uuid,
    live: true,
    call: { id: ev.call_id, tool: "", input: {}, needs: "none", reason: null, red: null, path: null, preview: null, how: null, command: null, out: "", err: "", started: null, record: null },
  };
  let call = base.call;
  switch (ev.type) {
    case "tool_call":
      call = { ...call, tool: ev.tool, input: ev.input, needs: ev.needs, reason: ev.reason, red: ev.red, path: ev.path, preview: ev.preview ?? null };
      break;
    case "tool_running":
      call = { ...call, how: ev.how, command: ev.command, started: Date.now() };
      break;
    case "tool_output": {
      if (ev.stream === "err") call = { ...call, err: (call.err + ev.text).slice(-LIVE_CAP) };
      else call = { ...call, out: (call.out + ev.text).slice(-LIVE_CAP) };
      break;
    }
    case "tool_result":
      call = { ...call, record: ev.record, how: ev.record.how ?? call.how, path: ev.record.path ?? call.path };
      break;
    default:
      return prev;
  }
  next.delete(ev.call_id);
  next.set(ev.call_id, { ...base, call, live: ev.type !== "tool_result" });
  // Only so many finished ones stay.
  const done = [...next.values()].filter((c) => !c.live);
  for (const c of done.slice(0, Math.max(0, done.length - KEEP_DONE))) next.delete(c.call.id);
  return next;
}

const Section = ({ title, count, children }: { title: string; count?: number; children: React.ReactNode }) => (
  <section className="space-y-3">
    <div className="flex items-center gap-2 text-zinc-400 font-bold uppercase tracking-widest text-xs">
      {title}
      {!!count && <span className="px-1.5 rounded-full bg-primary/15 text-primary text-[10px]">{count}</span>}
    </div>
    {children}
  </section>
);

const Empty = ({ children }: { children: React.ReactNode }) => <div className="text-[12px] text-zinc-500">{children}</div>;

const Who = ({ conn }: { conn: AgentConnection | undefined }) => {
  if (!conn) return <div className="text-[12px] text-zinc-500">An agent that has disconnected.</div>;
  return (
    <div className="space-y-1 text-[12px]">
      <div className="text-zinc-200">
        {conn.name ? <>{agentName(conn)} <span className="text-zinc-500">(the name it gives itself)</span></> : <span className="text-zinc-400">It gave no name.</span>}
      </div>
      {conn.chain.map((p, i) => (
        <div key={i} className="text-zinc-500 break-all" title={p.program || undefined}>
          {i === 0 ? "Program" : "Started by"}: <span className="text-zinc-300">{p.program ? programName(p.program) : "unknown"}</span> · process {p.pid}
        </div>
      ))}
    </div>
  );
};

interface RequestProps {
  req: AgentRequest;
  conn: AgentConnection | undefined;
  offered: GatewayOp[];
  longest: number;
  servers: ServerItem[];
  openSessions: Record<string, string>;
  onOpenServer: (uuid: string) => void;
  onError: (e: string | null) => void;
}

const RequestCard = ({ req, conn, offered, longest, servers, openSessions, onOpenServer, onError }: RequestProps) => {
  const fresh = useFreshGuard();
  // What it asked for that this version can grant; ones that change things start unchecked.
  const [ops, setOps] = useState<Set<GatewayOp>>(() => new Set(req.ops.filter((o) => offered.includes(o) && !OPS[o].changes)));
  const choices = useMemo(
    () =>
      servers
        .filter((s) => s.uuid && ((s.protocol ?? "ssh") === "ssh" || isFtp(s.protocol)))
        .sort((a, b) => Number(!!openSessions[b.uuid!]) - Number(!!openSessions[a.uuid!]) || a.name.localeCompare(b.name)),
    [servers, openSessions],
  );
  const [server, setServer] = useState<string>("");
  // An FTP server has files only: no command runs there.
  const ftp = isFtp(choices.find((s) => s.uuid === server)?.protocol);
  const given = [...ops].filter((o) => !(ftp && isCommandOp(o)));
  // The folders for the file operations, one a line: what the agent named, to start with.
  const [folders, setFolders] = useState(() => req.paths.join("\n"));
  const folderList = folders.split("\n").map((f) => f.trim()).filter(Boolean);
  const needsFolders = given.some(isFileOp);
  // The usual choices, and the time the agent asked for, up to the longest allowed.
  const durations = [...new Set([...DURATIONS, ...(req.minutes ? [req.minutes] : [])])].filter((d) => d <= longest).sort((a, b) => a - b);
  const [minutes, setMinutes] = useState(() => Math.min(req.minutes ?? 60, longest));
  const [label, setLabel] = useState("");
  const [denying, setDenying] = useState(false);
  const [note, setNote] = useState("");
  const [busy, setBusy] = useState(false);
  const unasked = offered.filter((o) => !req.ops.includes(o));
  const notOffered = req.ops.filter((o) => !offered.includes(o));
  const canApprove = !!server && !!openSessions[server] && given.length > 0 && (!needsFolders || folderList.length > 0) && !busy;

  const approve = async (e: React.MouseEvent) => {
    if (fresh(e) || !canApprove) return;
    setBusy(true);
    try {
      await gateway.approve(req.id, server, given, needsFolders ? folderList : [], "ask_each", minutes, label.trim() || null);
      onError(null);
    } catch (err) {
      onError(errText(err));
    } finally {
      setBusy(false);
    }
  };
  const deny = async (e: React.MouseEvent) => {
    if (fresh(e)) return;
    setBusy(true);
    try {
      await gateway.deny(req.id, note.trim() || null);
      onError(null);
    } catch (err) {
      onError(errText(err));
    } finally {
      setBusy(false);
    }
  };
  const toggle = (o: GatewayOp) =>
    setOps((s) => {
      const n = new Set(s);
      if (n.has(o)) n.delete(o);
      else n.add(o);
      return n;
    });

  return (
    <div data-agent-request={req.id} className="bg-[#121215] border border-primary/30 rounded-2xl p-4 space-y-4">
      <div className="flex items-start gap-3">
        <ShieldCheck size={18} className="text-primary shrink-0 mt-0.5" />
        <div className="min-w-0 flex-1 space-y-3">
          <div className="text-sm font-semibold text-zinc-100">An agent asks for access to a server</div>
          <Who conn={conn} />
          <div>
            <div className="text-[11px] uppercase tracking-wider text-zinc-500 mb-1">Why, in its words</div>
            <ExactText text={req.purpose} what="text" className="text-[12px] text-zinc-200" />
          </div>
        </div>
      </div>

      <div>
        <div className="text-[11px] uppercase tracking-wider text-zinc-500 mb-1.5">What it may do</div>
        <div className="space-y-1">
          {[...req.ops.filter((o) => offered.includes(o)), ...unasked].map((o) => {
            const no = ftp && isCommandOp(o);
            return (
              <label key={o} className={`flex items-center gap-2 text-[12px] ${no ? "text-zinc-600" : "text-zinc-300"}`}>
                <input type="checkbox" disabled={no} checked={ops.has(o) && !no} onChange={() => toggle(o)} />
                <span className={OPS[o].changes && !no ? "text-amber-200" : ""}>{OPS[o].label}</span>
                {no ? <span className="text-zinc-600">(an FTP server runs no commands)</span> : !req.ops.includes(o) && <span className="text-zinc-600">(not asked for)</span>}
              </label>
            );
          })}
          {notOffered.length > 0 && (
            <div className="text-[11px] text-zinc-500">Also asked for, not available in this version: {notOffered.map((o) => OPS[o].label.toLowerCase()).join(", ")}.</div>
          )}
        </div>
      </div>

      {needsFolders && (
        <label className="block">
          <div className="text-[11px] uppercase tracking-wider text-zinc-500 mb-1">Folders it may work in</div>
          <textarea
            data-agent-folders=""
            value={folders}
            onChange={(e) => setFolders(e.target.value)}
            rows={Math.min(6, Math.max(2, folderList.length + 1))}
            placeholder={"/var/www/site\n~/backups"}
            spellCheck={false}
            className="w-full px-2 py-1.5 bg-zinc-950 border border-white/10 rounded-lg text-[12px] font-mono text-zinc-200 focus:outline-none focus:border-primary/50"
          />
          <div className="text-[11px] text-zinc-500 mt-1">One a line. Its file operations work only inside these folders, as the server resolves them now; ~ is the login's own folder.</div>
          {ftp && (
            <div data-agent-ftp-links="" className="text-[11px] text-amber-200/90 mt-1">
              Some FTP servers show a link as a plain folder or file. If one of these folders holds a link to another place, the agent can reach files there through it.
            </div>
          )}
        </label>
      )}

      <div>
        <div className="text-[11px] uppercase tracking-wider text-zinc-500 mb-1.5">
          Which server{req.server_hint && <span className="normal-case tracking-normal text-zinc-400"> · it mentions “{req.server_hint}”</span>}
        </div>
        {choices.length === 0 && <Empty>No SSH or FTP server is saved.</Empty>}
        <div className="space-y-1 max-h-48 overflow-y-auto">
          {choices.map((s) => {
            const open = !!openSessions[s.uuid!];
            return (
              <div key={s.uuid} className="flex items-center gap-2 text-[12px]">
                <label className={`flex items-center gap-2 flex-1 min-w-0 ${open ? "text-zinc-200" : "text-zinc-500"}`}>
                  <input type="radio" name={`server-${req.id}`} disabled={!open} checked={server === s.uuid} onChange={() => setServer(s.uuid!)} />
                  <Server size={12} className="shrink-0" />
                  <span className="truncate">{s.name}</span>
                  {isFtp(s.protocol) && <span className="shrink-0 text-[10px] px-1 rounded border border-white/10 text-zinc-400">FTP</span>}
                </label>
                {!open && (
                  <button type="button" onClick={() => onOpenServer(s.uuid!)} className="h-7 px-2 rounded-md border border-white/10 text-[11px] text-zinc-300 hover:bg-white/5 flex items-center gap-1">
                    <Plug size={11} /> Open
                  </button>
                )}
              </div>
            );
          })}
        </div>
        <div className="text-[11px] text-zinc-500 mt-1">An agent works through the server's open session. Open it first; its questions (host key, certificate, password) come to you as always.</div>
      </div>

      <div className="grid sm:grid-cols-2 gap-3">
        <label className="block">
          <div className="text-[11px] uppercase tracking-wider text-zinc-500 mb-1">For how long</div>
          <select value={minutes} onChange={(e) => setMinutes(Number(e.target.value))} className="w-full h-8 px-2 bg-zinc-950 border border-white/10 rounded-lg text-[12px] text-zinc-200">
            {durations.map((d) => (
              <option key={d} value={d}>{minutesText(d)}</option>
            ))}
          </select>
        </label>
        <label className="block">
          <div className="text-[11px] uppercase tracking-wider text-zinc-500 mb-1">Name the agent sees</div>
          <input
            value={label}
            onChange={(e) => setLabel(e.target.value)}
            placeholder="The server's name (server-1 if it looks like an address)"
            maxLength={64}
            className="w-full h-8 px-2 bg-zinc-950 border border-white/10 rounded-lg text-[12px] text-zinc-200 focus:outline-none focus:border-primary/50"
          />
        </label>
      </div>
      <div className="text-[12px] text-zinc-400">{MODES.ask_each}. It never gets your passwords, keys or the server's address.</div>

      {denying ? (
        <div className="space-y-2">
          <input
            value={note}
            onChange={(e) => setNote(e.target.value)}
            placeholder="A short note for the agent (optional)"
            maxLength={500}
            className="w-full h-8 px-2 bg-zinc-950 border border-white/10 rounded-lg text-[12px] text-zinc-200 focus:outline-none focus:border-primary/50"
          />
          <div className="flex gap-2 justify-end">
            <button type="button" onClick={() => setDenying(false)} className="h-8 px-3 rounded-lg text-[11px] font-bold uppercase border border-white/10 text-zinc-300 hover:bg-white/5">Back</button>
            <button type="button" data-choice="deny" disabled={busy} onClick={(e) => void deny(e)} className="h-8 px-3 rounded-lg text-[11px] font-bold uppercase bg-red-500/10 border border-red-500/30 text-red-300 hover:bg-red-500/20 disabled:opacity-50 flex items-center gap-1">
              <X size={12} /> Deny
            </button>
          </div>
        </div>
      ) : (
        <div className="flex gap-2 justify-end">
          <button type="button" onClick={() => setDenying(true)} className="h-8 px-3 rounded-lg text-[11px] font-bold uppercase border border-white/10 text-zinc-300 hover:bg-white/5">Deny…</button>
          <button type="button" data-choice="approve" disabled={!canApprove} onClick={(e) => void approve(e)} className="h-8 px-3 rounded-lg text-[11px] font-bold uppercase bg-primary/10 border border-primary/30 text-primary hover:bg-primary/20 disabled:opacity-40 flex items-center gap-1">
            {busy ? <Loader2 size={12} className="animate-spin" /> : <Check size={12} />} Approve
          </button>
        </div>
      )}
    </div>
  );
};

const ConnectionBlock = ({ conn, grants, serverNames, onError }: { conn: AgentConnection; grants: GatewayOverview["grants"]; serverNames: Record<string, string>; onError: (e: string | null) => void }) => {
  const act = (p: Promise<void>) => p.then(() => onError(null), (e) => onError(errText(e)));
  return (
    <div data-agent-connection={conn.id} className="bg-[#121215] border border-white/5 rounded-2xl p-4 space-y-3">
      <div className="flex items-start gap-3">
        <div className="min-w-0 flex-1">
          <Who conn={conn} />
          <div className="text-[11px] text-zinc-600 mt-1">Connected at {timeText(conn.since)}{conn.bridged ? "" : " (not through submarine mcp)"}</div>
        </div>
        <button type="button" onClick={() => void act(gateway.disconnect(conn.id))} className="h-8 px-3 rounded-lg text-[11px] font-bold uppercase border border-white/10 text-zinc-300 hover:bg-white/5 flex items-center gap-1 shrink-0">
          <Unplug size={12} /> Disconnect
        </button>
      </div>
      {grants.length === 0 && <div className="text-[12px] text-zinc-500">No access given.</div>}
      {grants.map((g) => (
        <div key={g.id} data-agent-grant={g.id} className="flex items-start gap-3 border-t border-white/5 pt-3">
          <div className="min-w-0 flex-1 text-[12px] space-y-0.5">
            <div className="text-zinc-200">
              {serverNames[g.server_uuid] ?? "A removed server"}
              {g.label !== serverNames[g.server_uuid] && <span className="text-zinc-500"> (the agent sees “{g.label}”)</span>}
            </div>
            <div className="text-zinc-500">{g.ops.map((o) => OPS[o].label).join(" · ")}</div>
            {g.folders.length > 0 && <div className="text-zinc-500 break-all" dir="ltr">{g.folders.join(" · ")}</div>}
            <div className="text-zinc-500">{MODES[g.mode]} · {minutesText(g.minutes_left)} left</div>
          </div>
          <button type="button" onClick={() => void act(gateway.revoke(g.id))} className="h-8 px-3 rounded-lg text-[11px] font-bold uppercase bg-red-500/10 border border-red-500/30 text-red-300 hover:bg-red-500/20 shrink-0">
            Revoke
          </button>
        </div>
      ))}
    </div>
  );
};

const CallBlock = ({ item, conn, serverNames }: { item: CallItem; conn: AgentConnection | undefined; serverNames: Record<string, string> }) => (
  <div data-agent-call={item.call.id} className="space-y-1">
    <div className="text-[11px] text-zinc-500">
      {agentName(conn)} on {serverNames[item.serverUuid] ?? "a removed server"}
    </div>
    <ToolCard call={item.call} live={item.live} canAlways={false} onDecide={(answer) => gateway.decide(item.call.id, answer)} />
  </div>
);
