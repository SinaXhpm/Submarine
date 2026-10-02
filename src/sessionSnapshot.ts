// Open-session snapshot: a plain-JSON, localStorage-backed record of which
// servers were open with which terminal tabs, so a restart, crash or
// accidental close can reopen everything with custom names intact. Pure
// build/parse helpers live here — DesktopApp keeps only orchestration
// (when to save, when to stage). Anything off-shape is dropped: the
// snapshot is untrusted input on every read.
export interface SnapTerm {
  title: string;
  container?: { name: string; useSudo: boolean };
}

export interface SnapSession {
  serverId: number;
  customName?: string | null;
  terminals?: SnapTerm[];
  activeIndex?: number;
}

export interface SessionSnapshot {
  version: 1;
  savedAt: number;
  activeServerId: number | null;
  sessions: SnapSession[];
}

export const MAX_SNAP_SESSIONS = 20;
export const MAX_SNAP_TERMS = 10;

export const snapshotKey = (profile: string) => `submarine-open-sessions.v1.${profile}`;

// Quick-connect sessions (serverId 0) are left out — their credentials live
// only in memory and can't be re-dialled, so restoring them is impossible.
export function buildSnapshot(opts: {
  sessions: { id: string; serverId: number }[];
  terminalsBySession: Record<string, { id: string; title: string; container?: { name: string; useSudo: boolean } }[]>;
  activeTermBySession: Record<string, string>;
  tabNames: Record<string, string>;
  activeView: string;
}): SessionSnapshot {
  const sessions = opts.sessions
    .filter(s => typeof s.serverId === 'number' && s.serverId > 0)
    .map(s => {
      const terms = (opts.terminalsBySession[s.id] ?? []).slice(0, MAX_SNAP_TERMS).map(t => ({
        title: t.title,
        ...(t.container ? { container: { name: t.container.name, useSudo: t.container.useSudo } } : {}),
      }));
      const ids = (opts.terminalsBySession[s.id] ?? []).map(t => t.id);
      return {
        serverId: s.serverId,
        customName: opts.tabNames[s.id] ?? null,
        terminals: terms.length > 0 ? terms : [{ title: '1' }],
        activeIndex: Math.max(0, ids.indexOf(opts.activeTermBySession[s.id])),
      };
    });
  return {
    version: 1,
    savedAt: Date.now(),
    activeServerId: opts.sessions.find(s => s.id === opts.activeView)?.serverId ?? null,
    sessions,
  };
}

export function parseSnapshot(raw: string | null, hasServer: (id: number) => boolean): {
  found: boolean;
  total: number;
  savedIds: string;
  sessions: SnapSession[];
  activeServerId: number | null;
} {
  const empty = { found: false, total: 0, savedIds: "[]", sessions: [] as SnapSession[], activeServerId: null as number | null };
  let snap: any = null;
  try {
    snap = raw ? JSON.parse(raw) : null;
  } catch {
    return { ...empty, found: !!raw };
  }
  if (!snap || !Array.isArray(snap.sessions)) return { ...empty, found: !!raw };
  let savedIds = "[]";
  try {
    savedIds = JSON.stringify(snap.sessions.map((r: any) => r?.serverId));
  } catch { /* keep default */ }
  const sessions = snap.sessions.filter((r: any) =>
    r && typeof r.serverId === 'number' && r.serverId > 0 && hasServer(r.serverId)).slice(0, MAX_SNAP_SESSIONS);
  return {
    found: true,
    total: snap.sessions.length,
    savedIds,
    sessions,
    activeServerId: typeof snap.activeServerId === 'number' && hasServer(snap.activeServerId) ? snap.activeServerId : null,
  };
}
